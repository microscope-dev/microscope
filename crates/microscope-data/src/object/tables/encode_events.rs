use super::super::codec::{
    Coding, NO_KEY_COLUMN, SectionWriter, WHOLE_SECTION_GROUP, delta_encode_i64, delta_encode_u64,
    encode_dict_u64, relative_encode_i64, zigzag_encode,
};
use super::super::{EncodedSection, ObjectError, SectionKind, TimeUnit};
use super::encode_syscalls::{encode_syscall_blobs, encode_syscall_stats, encode_syscalls};
use super::{TABLE_VERSION, add_u64_col, count_u32};
use crate::capture::Capture;

#[expect(
    clippy::too_many_lines,
    reason = "encodes each event and syscall metadata table section in one sequence"
)]
pub(super) fn encode_event_tables(
    capture: &Capture,
    time_unit: TimeUnit,
    zstd_level: i32,
    key: u64,
    sections: &mut Vec<EncodedSection>,
) -> Result<(), ObjectError> {
    if !capture.sites.is_empty() {
        let mut w = SectionWriter::new(
            SectionKind::Sites,
            TABLE_VERSION,
            TimeUnit::Ns,
            count_u32(capture.sites.len())?,
            0,
            zstd_level,
        );
        let names: Vec<u64> = capture
            .sites
            .iter()
            .map(|r| u64::from(r.name.get()))
            .collect();
        let files: Vec<u64> = capture
            .sites
            .iter()
            .map(|r| u64::from(r.file.get()))
            .collect();
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
            Coding::String,
            NO_KEY_COLUMN,
            false,
            &files,
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            2,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &lines,
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            3,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_u64(&mods),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            4,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &kinds,
        )?;
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
        let vals: Vec<u64> = capture
            .zones
            .iter()
            .map(|r| u64::from(r.value.get()))
            .collect();
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            0,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_i64(&starts, base),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            1,
            Coding::Relative,
            0,
            false,
            &relative_encode_i64(&ends, &starts),
        )?;
        w.add_column(
            WHOLE_SECTION_GROUP,
            2,
            Coding::Dictionary,
            NO_KEY_COLUMN,
            0,
            &encode_dict_u64(&sites),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            3,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &flags,
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            4,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &vals,
        )?;
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
            &depths,
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            2,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &counts,
        )?;
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
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_i64(&times, base),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            1,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_u64(&vals),
        )?;
        w.add_column(
            WHOLE_SECTION_GROUP,
            2,
            Coding::Dictionary,
            NO_KEY_COLUMN,
            0,
            &encode_dict_u64(&sites),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            3,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_u64(&threads),
        )?;
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
        let texts: Vec<u64> = capture
            .markers
            .iter()
            .map(|r| u64::from(r.text.get()))
            .collect();
        let kinds: Vec<u64> = capture.markers.iter().map(|r| u64::from(r.kind)).collect();
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            0,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_i64(&times, base),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            1,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_u64(&threads),
        )?;
        w.add_column(
            WHOLE_SECTION_GROUP,
            2,
            Coding::Dictionary,
            NO_KEY_COLUMN,
            0,
            &encode_dict_u64(&sites),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            3,
            Coding::String,
            NO_KEY_COLUMN,
            false,
            &texts,
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            4,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &kinds,
        )?;
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
        let runtimes: Vec<u64> = capture
            .tasks
            .iter()
            .map(|r| u64::from(r.runtime.get()))
            .collect();
        let numbers: Vec<u64> = capture.tasks.iter().map(|r| r.number.get()).collect();
        let roots: Vec<u64> = capture
            .tasks
            .iter()
            .map(|r| u64::from(r.root.get()))
            .collect();
        let labels: Vec<u64> = capture
            .tasks
            .iter()
            .map(|r| u64::from(r.labels.get()))
            .collect();
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
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            0,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &runtimes,
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
            &delta_encode_u64(&roots),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            3,
            Coding::String,
            NO_KEY_COLUMN,
            false,
            &labels,
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            4,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_i64(&first, base),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            5,
            Coding::Relative,
            4,
            false,
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
            .map(|r| u64::from(r.stack.get()))
            .collect();
        let states: Vec<u64> = capture
            .task_waits
            .iter()
            .map(|r| u64::from(r.state.get()))
            .collect();
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            0,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
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
            &delta_encode_u64(&tasks),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            3,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_u64(&stacks),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            4,
            Coding::String,
            NO_KEY_COLUMN,
            false,
            &states,
        )?;
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
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_i64(&starts, base),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            1,
            Coding::Relative,
            0,
            false,
            &relative_encode_i64(&ends, &starts),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            2,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &reasons,
        )?;
        sections.push(w.finish(key)?);
    }

    if !capture.syscalls.is_empty() {
        sections.push(encode_syscalls(capture, time_unit, zstd_level, key)?);
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
        let texts: Vec<u64> = capture
            .syscall_notes
            .iter()
            .map(|r| u64::from(r.text.get()))
            .collect();
        let stacks: Vec<u64> = capture
            .syscall_notes
            .iter()
            .map(|r| u64::from(r.stack.get()))
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
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_i64(&times, base),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            1,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &values,
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            2,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &extras,
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            3,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_u64(&threads),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            4,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_u64(&procs),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            5,
            Coding::String,
            NO_KEY_COLUMN,
            false,
            &texts,
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            6,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_u64(&stacks),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            7,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &blens,
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            8,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_u64(&calls),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            9,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &kinds,
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            10,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &flags,
        )?;
        sections.push(w.finish(key)?);
    }

    if !capture.syscall_stats.is_empty() {
        sections.push(encode_syscall_stats(capture, zstd_level, key)?);
    }

    if !capture.syscall_errnos.is_empty() {
        let mut w = SectionWriter::new(
            SectionKind::SyscallErrnos,
            TABLE_VERSION,
            TimeUnit::Ns,
            count_u32(capture.syscall_errnos.len())?,
            0,
            zstd_level,
        );
        let counts: Vec<u64> = capture
            .syscall_errnos
            .iter()
            .map(|r| r.count.get())
            .collect();
        let errnos: Vec<u64> = capture
            .syscall_errnos
            .iter()
            .map(|r| u64::from(r.errno.get()))
            .collect();
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            0,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &counts,
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            1,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &errnos,
        )?;
        sections.push(w.finish(key)?);
    }

    Ok(())
}
