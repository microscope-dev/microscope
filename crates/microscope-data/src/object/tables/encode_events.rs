use super::super::codec::{
    Coding, NO_KEY_COLUMN, SectionWriter, WHOLE_SECTION_GROUP, delta_encode_i64, encode_dict_u64,
    relative_encode_i64, zigzag_encode,
};
use super::super::{EncodedSection, ObjectError, SectionKind, TimeUnit};
use super::encode_syscalls::{
    encode_syscall_blobs, encode_syscall_stats_and_errnos, encode_syscalls,
};
use super::{
    StringEncodeMap, TABLE_VERSION, add_delta_u64_col, add_plain_u64_col, add_u64_col, count_u32,
    remap_stack,
};
use crate::capture::Capture;

#[expect(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    reason = "encodes each event and syscall metadata table section in one sequence"
)]
pub(super) fn encode_event_tables(
    capture: &Capture,
    str_map: &StringEncodeMap,
    old_to_dfs: &[u32],
    time_unit: TimeUnit,
    zstd_level: i32,
    key: u64,
    include_sites: bool,
    sections: &mut Vec<EncodedSection>,
) -> Result<(), ObjectError> {
    if include_sites && !capture.sites.is_empty() {
        let mut w = SectionWriter::new(
            SectionKind::Sites,
            TABLE_VERSION,
            TimeUnit::Ns,
            count_u32(capture.sites.len())?,
            0,
            zstd_level,
        );
        let names = str_map.encode_refs(capture.sites.iter().map(|r| r.name.get()))?;
        let files = str_map.encode_refs(capture.sites.iter().map(|r| r.file.get()))?;
        let lines: Vec<u64> = capture
            .sites
            .iter()
            .map(|r| u64::from(r.line.get()))
            .collect();
        let mods: Vec<u64> = capture
            .sites
            .iter()
            .map(|r| u64::from(r.module.get()))
            .collect();
        let kinds: Vec<u64> = capture.sites.iter().map(|r| u64::from(r.kind)).collect();
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 0, &names)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &files)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 2, &lines)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 3, &mods)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 4, &kinds)?;
        sections.push(w.finish(key)?);
    }

    if !capture.zones.is_empty() {
        let base = capture
            .zones
            .first()
            .map_or(0, |z| time_unit.to_units(z.start.get()));
        let mut w = SectionWriter::new(
            SectionKind::Zones,
            TABLE_VERSION,
            time_unit,
            count_u32(capture.zones.len())?,
            base,
            zstd_level,
        );
        let starts: Vec<i64> = capture
            .zones
            .iter()
            .map(|r| time_unit.to_units(r.start.get()))
            .collect();
        let ends: Vec<i64> = capture
            .zones
            .iter()
            .map(|r| time_unit.to_units(r.end.get()))
            .collect();
        let sites: Vec<u64> = capture
            .zones
            .iter()
            .map(|r| u64::from(r.site.get()))
            .collect();
        let flags: Vec<u64> = capture
            .zones
            .iter()
            .map(|r| u64::from(r.flags.get()))
            .collect();
        let vals = str_map.encode_refs(capture.zones.iter().map(|r| r.value.get()))?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            0,
            Coding::VarintDelta,
            NO_KEY_COLUMN,
            &delta_encode_i64(&starts, base),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            1,
            Coding::VarintRelative,
            0,
            &relative_encode_i64(&ends, &starts),
        )?;
        w.add_column(
            WHOLE_SECTION_GROUP,
            2,
            Coding::DictVarint,
            NO_KEY_COLUMN,
            &encode_dict_u64(&sites),
        )?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 3, &flags)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 4, &vals)?;
        sections.push(w.finish(key)?);
    }

    if !capture.zone_ranges.is_empty() {
        let mut w = SectionWriter::new(
            SectionKind::ZoneRanges,
            TABLE_VERSION,
            TimeUnit::Ns,
            count_u32(capture.zone_ranges.len())?,
            0,
            zstd_level,
        );
        let threads: Vec<u64> = capture
            .zone_ranges
            .iter()
            .map(|r| u64::from(r.thread.get()))
            .collect();
        let depths: Vec<u64> = capture
            .zone_ranges
            .iter()
            .map(|r| u64::from(r.depth.get()))
            .collect();
        let counts: Vec<u64> = capture
            .zone_ranges
            .iter()
            .map(|r| u64::from(r.count.get()))
            .collect();
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 0, &threads)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &depths)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 2, &counts)?;
        sections.push(w.finish(key)?);
    }

    if !capture.plots.is_empty() {
        let base = capture
            .plots
            .first()
            .map_or(0, |p| time_unit.to_units(p.time.get()));
        let mut w = SectionWriter::new(
            SectionKind::Plots,
            TABLE_VERSION,
            time_unit,
            count_u32(capture.plots.len())?,
            base,
            zstd_level,
        );
        let times: Vec<i64> = capture
            .plots
            .iter()
            .map(|r| time_unit.to_units(r.time.get()))
            .collect();
        let vals: Vec<u64> = capture.plots.iter().map(|r| r.value.get()).collect();
        let sites: Vec<u64> = capture
            .plots
            .iter()
            .map(|r| u64::from(r.site.get()))
            .collect();
        let threads: Vec<u64> = capture
            .plots
            .iter()
            .map(|r| u64::from(r.thread.get()))
            .collect();
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            0,
            Coding::VarintDelta,
            NO_KEY_COLUMN,
            &delta_encode_i64(&times, base),
        )?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &vals)?;
        w.add_column(
            WHOLE_SECTION_GROUP,
            2,
            Coding::DictVarint,
            NO_KEY_COLUMN,
            &encode_dict_u64(&sites),
        )?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 3, &threads)?;
        sections.push(w.finish(key)?);
    }

    if !capture.markers.is_empty() {
        let base = capture
            .markers
            .first()
            .map_or(0, |m| time_unit.to_units(m.time.get()));
        let mut w = SectionWriter::new(
            SectionKind::Markers,
            TABLE_VERSION,
            time_unit,
            count_u32(capture.markers.len())?,
            base,
            zstd_level,
        );
        let times: Vec<i64> = capture
            .markers
            .iter()
            .map(|r| time_unit.to_units(r.time.get()))
            .collect();
        let threads: Vec<u64> = capture
            .markers
            .iter()
            .map(|r| u64::from(r.thread.get()))
            .collect();
        let sites: Vec<u64> = capture
            .markers
            .iter()
            .map(|r| u64::from(r.site.get()))
            .collect();
        let texts = str_map.encode_refs(capture.markers.iter().map(|r| r.text.get()))?;
        let kinds: Vec<u64> = capture.markers.iter().map(|r| u64::from(r.kind)).collect();
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            0,
            Coding::VarintDelta,
            NO_KEY_COLUMN,
            &delta_encode_i64(&times, base),
        )?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &threads)?;
        w.add_column(
            WHOLE_SECTION_GROUP,
            2,
            Coding::DictVarint,
            NO_KEY_COLUMN,
            &encode_dict_u64(&sites),
        )?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 3, &texts)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 4, &kinds)?;
        sections.push(w.finish(key)?);
    }

    if !capture.tasks.is_empty() {
        let base = capture
            .tasks
            .first()
            .map_or(0, |t| time_unit.to_units(t.first_seen.get()));
        let mut w = SectionWriter::new(
            SectionKind::Tasks,
            TABLE_VERSION,
            time_unit,
            count_u32(capture.tasks.len())?,
            base,
            zstd_level,
        );
        let runtimes = str_map.encode_refs(capture.tasks.iter().map(|r| r.runtime.get()))?;
        let numbers: Vec<u64> = capture.tasks.iter().map(|r| r.number.get()).collect();
        let roots: Vec<u64> = capture
            .tasks
            .iter()
            .map(|r| u64::from(r.root.get()))
            .collect();
        let labels = str_map.encode_refs(capture.tasks.iter().map(|r| r.labels.get()))?;
        let first: Vec<i64> = capture
            .tasks
            .iter()
            .map(|r| time_unit.to_units(r.first_seen.get()))
            .collect();
        let last: Vec<i64> = capture
            .tasks
            .iter()
            .map(|r| time_unit.to_units(r.last_seen.get()))
            .collect();
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 0, &runtimes)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &numbers)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 2, &roots)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 3, &labels)?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            4,
            Coding::VarintDelta,
            NO_KEY_COLUMN,
            &delta_encode_i64(&first, base),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            5,
            Coding::VarintRelative,
            4,
            &relative_encode_i64(&last, &first),
        )?;
        sections.push(w.finish(key)?);
    }

    if !capture.task_waits.is_empty() {
        let base = capture
            .task_waits
            .first()
            .map_or(0, |t| time_unit.to_units(t.time.get()));
        let mut w = SectionWriter::new(
            SectionKind::TaskWaits,
            TABLE_VERSION,
            time_unit,
            count_u32(capture.task_waits.len())?,
            base,
            zstd_level,
        );
        let times: Vec<i64> = capture
            .task_waits
            .iter()
            .map(|r| time_unit.to_units(r.time.get()))
            .collect();
        let weights: Vec<u64> = capture.task_waits.iter().map(|r| r.weight.get()).collect();
        let tasks: Vec<u64> = capture
            .task_waits
            .iter()
            .map(|r| u64::from(r.task.get()))
            .collect();
        let stacks: Vec<u64> = capture
            .task_waits
            .iter()
            .map(|r| u64::from(remap_stack(r.stack.get(), old_to_dfs)))
            .collect();
        let states = str_map.encode_refs(capture.task_waits.iter().map(|r| r.state.get()))?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            0,
            Coding::VarintDelta,
            NO_KEY_COLUMN,
            &delta_encode_i64(&times, base),
        )?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &weights)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 2, &tasks)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 3, &stacks)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 4, &states)?;
        sections.push(w.finish(key)?);
    }

    if !capture.gaps.is_empty() {
        let base = capture
            .gaps
            .first()
            .map_or(0, |g| time_unit.to_units(g.start.get()));
        let mut w = SectionWriter::new(
            SectionKind::Gaps,
            TABLE_VERSION,
            time_unit,
            count_u32(capture.gaps.len())?,
            base,
            zstd_level,
        );
        let starts: Vec<i64> = capture
            .gaps
            .iter()
            .map(|r| time_unit.to_units(r.start.get()))
            .collect();
        let ends: Vec<i64> = capture
            .gaps
            .iter()
            .map(|r| time_unit.to_units(r.end.get()))
            .collect();
        let reasons: Vec<u64> = capture.gaps.iter().map(|r| u64::from(r.reason)).collect();
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            0,
            Coding::VarintDelta,
            NO_KEY_COLUMN,
            &delta_encode_i64(&starts, base),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            1,
            Coding::VarintRelative,
            0,
            &relative_encode_i64(&ends, &starts),
        )?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 2, &reasons)?;
        sections.push(w.finish(key)?);
    }

    if !capture.syscalls.is_empty() {
        sections.push(encode_syscalls(
            capture, old_to_dfs, time_unit, zstd_level, key,
        )?);
    }

    if !capture.syscall_blobs.is_empty() {
        sections.push(encode_syscall_blobs(capture, zstd_level, key)?);
    }

    if !capture.syscall_notes.is_empty() {
        let base = capture
            .syscall_notes
            .first()
            .map_or(0, |n| time_unit.to_units(n.time.get()));
        let mut w = SectionWriter::new(
            SectionKind::SyscallNotes,
            TABLE_VERSION,
            time_unit,
            count_u32(capture.syscall_notes.len())?,
            base,
            zstd_level,
        );
        let times: Vec<i64> = capture
            .syscall_notes
            .iter()
            .map(|r| time_unit.to_units(r.time.get()))
            .collect();
        let values: Vec<u64> = capture
            .syscall_notes
            .iter()
            .map(|r| zigzag_encode(r.value.get()))
            .collect();
        let extras: Vec<u64> = capture
            .syscall_notes
            .iter()
            .map(|r| r.extra.get())
            .collect();
        let threads: Vec<u64> = capture
            .syscall_notes
            .iter()
            .map(|r| u64::from(r.thread.get()))
            .collect();
        let procs: Vec<u64> = capture
            .syscall_notes
            .iter()
            .map(|r| u64::from(r.process.get()))
            .collect();
        let texts = str_map.encode_refs(capture.syscall_notes.iter().map(|r| r.text.get()))?;
        let stacks: Vec<u64> = capture
            .syscall_notes
            .iter()
            .map(|r| u64::from(remap_stack(r.stack.get(), old_to_dfs)))
            .collect();
        let blens: Vec<u64> = capture
            .syscall_notes
            .iter()
            .map(|r| u64::from(r.blob_len.get()))
            .collect();
        let calls: Vec<u64> = capture
            .syscall_notes
            .iter()
            .map(|r| u64::from(r.calls.get()))
            .collect();
        let kinds: Vec<u64> = capture
            .syscall_notes
            .iter()
            .map(|r| u64::from(r.kind))
            .collect();
        let flags: Vec<u64> = capture
            .syscall_notes
            .iter()
            .map(|r| u64::from(r.flags))
            .collect();
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            0,
            Coding::VarintDelta,
            NO_KEY_COLUMN,
            &delta_encode_i64(&times, base),
        )?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &values)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 2, &extras)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 3, &threads)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 4, &procs)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 5, &texts)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 6, &stacks)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 7, &blens)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 8, &calls)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 9, &kinds)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 10, &flags)?;
        sections.push(w.finish(key)?);
    }

    if !capture.syscall_stats.is_empty() || !capture.syscall_errnos.is_empty() {
        let (stats_sec, errnos_sec) = encode_syscall_stats_and_errnos(capture, zstd_level, key)?;
        if let Some(sec) = stats_sec {
            sections.push(sec);
        }
        if let Some(sec) = errnos_sec {
            sections.push(sec);
        }
    }

    Ok(())
}
