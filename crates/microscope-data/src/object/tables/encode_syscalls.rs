use std::collections::BTreeMap;

use super::super::codec::{
    Coding, NO_KEY_COLUMN, SectionWriter, WHOLE_SECTION_GROUP, delta_by_key_encode_u64,
    delta_encode_i64, delta_encode_u64, encode_dict_u64, encode_words, zigzag_encode,
};
use super::super::{EncodedSection, ObjectError, SectionKind, TimeUnit};
use super::{NOTE_BLOBS_ARGUMENT, TABLE_VERSION, add_u64_col, count_u32, syscall_arg_count};
use crate::capture::Capture;
use crate::capture::records::{Syscall, UNSEEN, captures as parse_captures};
use crate::syscalls::{Capture as ArgCapture, CaptureState, ReadAt};

#[expect(
    clippy::too_many_lines,
    reason = "encodes grouped syscall columns per abi and number"
)]
pub(super) fn encode_syscalls(
    capture: &Capture,
    time_unit: TimeUnit,
    zstd_level: i32,
    key: u64,
) -> Result<EncodedSection, ObjectError> {
    let mut by_kind: BTreeMap<(u8, u16), Vec<&Syscall>> = BTreeMap::new();
    let mut keys_per_row = Vec::with_capacity(capture.syscalls.len());
    for call in &capture.syscalls {
        let nr = u16::try_from(call.number.get())
            .map_err(|_| ObjectError::Layout("syscall number exceeds u16".into()))?;
        let gk = (call.abi, nr);
        keys_per_row.push(gk);
        by_kind.entry(gk).or_default().push(call);
    }

    let group_index: BTreeMap<(u8, u16), u64> = by_kind
        .keys()
        .enumerate()
        .map(|(idx, &gk)| (gk, idx as u64))
        .collect();
    let order: Vec<u64> = keys_per_row
        .iter()
        .map(|gk| group_index.get(gk).copied().unwrap_or(0))
        .collect();

    let first_time = capture
        .syscalls
        .iter()
        .find_map(|c| {
            let t = if c.exited.get() != UNSEEN {
                c.exited.get()
            } else if c.entered.get() != UNSEEN {
                c.entered.get()
            } else {
                return None;
            };
            Some(time_unit.to_units(t))
        })
        .unwrap_or(0);

    let mut exit_times = Vec::with_capacity(capture.syscalls.len());
    let mut prev_exit = first_time;
    for call in &capture.syscalls {
        let t = if call.exited.get() != UNSEEN {
            time_unit.to_units(call.exited.get())
        } else if call.entered.get() != UNSEEN {
            time_unit.to_units(call.entered.get())
        } else {
            prev_exit
        };
        exit_times.push(t);
        prev_exit = t;
    }

    let mut w = SectionWriter::new(
        SectionKind::Syscalls,
        TABLE_VERSION,
        time_unit,
        count_u32(capture.syscalls.len())?,
        first_time,
        zstd_level,
    );
    add_u64_col(
        &mut w,
        WHOLE_SECTION_GROUP,
        0,
        Coding::Order,
        NO_KEY_COLUMN,
        false,
        &order,
    )?;
    add_u64_col(
        &mut w,
        WHOLE_SECTION_GROUP,
        1,
        Coding::Delta,
        NO_KEY_COLUMN,
        true,
        &delta_encode_i64(&exit_times, first_time),
    )?;

    for ((abi, nr), rows) in by_kind {
        let g = w.add_group(abi, u8::MAX, nr, count_u32(rows.len())?)?;
        let arg_count = syscall_arg_count(abi, u32::from(nr));

        let mut durations = Vec::with_capacity(rows.len());
        let mut threads_u32 = Vec::with_capacity(rows.len());
        let mut threads = Vec::with_capacity(rows.len());
        let mut rets = Vec::with_capacity(rows.len());
        let mut stacks = Vec::with_capacity(rows.len());
        let mut cap_lens = Vec::with_capacity(rows.len());
        let mut outcomes = Vec::with_capacity(rows.len());
        let mut flags = Vec::with_capacity(rows.len());
        let mut unseen = Vec::with_capacity(rows.len());
        let mut any_unseen = false;
        let mut args: Vec<Vec<u64>> = (0..arg_count)
            .map(|_| Vec::with_capacity(rows.len()))
            .collect();

        for call in &rows {
            let entered = call.entered.get();
            let exited = call.exited.get();
            let mask = u8::from(entered == UNSEEN) | (u8::from(exited == UNSEEN) << 1);
            any_unseen |= mask != 0;
            unseen.push(u64::from(mask));
            let dur = if mask == 0 {
                time_unit
                    .to_units(exited)
                    .wrapping_sub(time_unit.to_units(entered))
            } else {
                0
            };
            durations.push(zigzag_encode(dur));
            let tid = call.thread.get();
            threads_u32.push(tid);
            threads.push(u64::from(tid));
            rets.push(call.ret.get().cast_unsigned());
            stacks.push(u64::from(call.stack.get()));
            cap_lens.push(u64::from(call.captures_len.get()));
            outcomes.push(u64::from(call.outcome));
            flags.push(u64::from(call.flags));
            for (i, col) in args.iter_mut().enumerate() {
                if let Some(arg) = call.args.get(i) {
                    col.push(arg.get());
                }
            }
        }

        add_u64_col(&mut w, g, 0, Coding::Plain, NO_KEY_COLUMN, true, &durations)?;
        add_u64_col(&mut w, g, 1, Coding::Plain, NO_KEY_COLUMN, false, &threads)?;
        add_u64_col(
            &mut w,
            g,
            2,
            Coding::DeltaByKey,
            1,
            false,
            &delta_by_key_encode_u64(&rets, &threads_u32),
        )?;
        add_u64_col(
            &mut w,
            g,
            3,
            Coding::DeltaByKey,
            1,
            false,
            &delta_by_key_encode_u64(&stacks, &threads_u32),
        )?;
        add_u64_col(&mut w, g, 4, Coding::Plain, NO_KEY_COLUMN, false, &cap_lens)?;
        add_u64_col(&mut w, g, 5, Coding::Plain, NO_KEY_COLUMN, false, &outcomes)?;
        add_u64_col(&mut w, g, 6, Coding::Plain, NO_KEY_COLUMN, false, &flags)?;
        if any_unseen {
            add_u64_col(&mut w, g, 7, Coding::Plain, NO_KEY_COLUMN, false, &unseen)?;
        }
        for (i, col) in args.iter().enumerate() {
            let col_id = u8::try_from(10 + i).unwrap_or(15);
            add_u64_col(
                &mut w,
                g,
                col_id,
                Coding::DeltaByKey,
                1,
                false,
                &delta_by_key_encode_u64(col, &threads_u32),
            )?;
        }
    }

    w.finish(key)
}

#[expect(
    clippy::too_many_lines,
    reason = "encodes grouped syscall capture blobs and note blobs"
)]
pub(super) fn encode_syscall_blobs(
    capture: &Capture,
    zstd_level: i32,
    key: u64,
) -> Result<EncodedSection, ObjectError> {
    let mut call_counts = Vec::with_capacity(capture.syscalls.len());
    let mut call_arg_order = Vec::new();
    let mut by_arg: BTreeMap<(u8, u16, u8), Vec<ArgCapture>> = BTreeMap::new();
    let mut total_captures = 0_usize;

    for call in &capture.syscalls {
        let bytes = capture.blob(call.captures.get(), call.captures_len.get());
        if bytes.is_empty() {
            call_counts.push(0_u64);
            continue;
        }
        let parsed = parse_captures(bytes)
            .ok_or_else(|| ObjectError::Capture("malformed syscall capture bytes".into()))?;
        let nr = u16::try_from(call.number.get())
            .map_err(|_| ObjectError::Layout("syscall number exceeds u16".into()))?;
        call_counts.push(parsed.len() as u64);
        total_captures += parsed.len();
        for cap in parsed {
            let gk = (call.abi, nr, cap.arg);
            call_arg_order.push(gk);
            by_arg.entry(gk).or_default().push(cap);
        }
    }

    let mut note_blobs: Vec<&[u8]> = Vec::new();
    for note in &capture.syscall_notes {
        let bytes = capture.blob(note.blob.get(), note.blob_len.get());
        if !bytes.is_empty() {
            note_blobs.push(bytes);
        }
    }

    let group_index: BTreeMap<(u8, u16, u8), u64> = by_arg
        .keys()
        .enumerate()
        .map(|(idx, &gk)| (gk, idx as u64))
        .collect();
    let order: Vec<u64> = call_arg_order
        .iter()
        .map(|gk| group_index.get(gk).copied().unwrap_or(0))
        .collect();

    let mut w = SectionWriter::new(
        SectionKind::SyscallBlobs,
        TABLE_VERSION,
        TimeUnit::Ns,
        count_u32(total_captures)?,
        0,
        zstd_level,
    );
    add_u64_col(
        &mut w,
        WHOLE_SECTION_GROUP,
        0,
        Coding::Plain,
        NO_KEY_COLUMN,
        false,
        &call_counts,
    )?;
    add_u64_col(
        &mut w,
        WHOLE_SECTION_GROUP,
        1,
        Coding::Order,
        NO_KEY_COLUMN,
        false,
        &order,
    )?;

    for ((abi, nr, arg), caps) in &by_arg {
        let g = w.add_group(*abi, *arg, *nr, count_u32(caps.len())?)?;
        let mut headers = Vec::with_capacity(caps.len());
        let mut wanted = Vec::with_capacity(caps.len());
        let mut lengths = Vec::with_capacity(caps.len());

        for cap in caps {
            let when = match cap.when {
                ReadAt::Entry => 0_u64,
                ReadAt::Exit => 1_u64,
            };
            let (state, want) = match cap.state {
                CaptureState::Complete => (0_u64, 0_u64),
                CaptureState::Truncated { wanted } => (1_u64, u64::from(wanted)),
                CaptureState::Unreadable => (2_u64, 0_u64),
                CaptureState::NotRequested => (3_u64, 0_u64),
            };
            headers.push(u64::from(cap.arg) | (when << 8) | (state << 16));
            wanted.push(want);
            lengths.push(cap.bytes.len() as u64);
        }

        w.add_column(
            g,
            0,
            Coding::Dictionary,
            NO_KEY_COLUMN,
            0,
            &encode_dict_u64(&headers),
        )?;
        add_u64_col(&mut w, g, 1, Coding::Plain, NO_KEY_COLUMN, false, &wanted)?;
        add_u64_col(&mut w, g, 2, Coding::Plain, NO_KEY_COLUMN, false, &lengths)?;

        let first_len = caps.first().map_or(0, |c| c.bytes.len());
        let fixed_words = first_len > 0
            && first_len.is_multiple_of(8)
            && caps.iter().all(|c| c.bytes.len() == first_len);
        if fixed_words {
            let refs: Vec<&[u8]> = caps.iter().map(|c| c.bytes.as_ref()).collect();
            let transposed = encode_words(&refs, first_len);
            w.add_column(g, 3, Coding::Words, NO_KEY_COLUMN, 0, &transposed)?;
        } else {
            let mut joined = Vec::with_capacity(caps.iter().map(|c| c.bytes.len()).sum());
            for cap in caps {
                joined.extend_from_slice(&cap.bytes);
            }
            w.add_column(g, 3, Coding::Bytes, NO_KEY_COLUMN, 0, &joined)?;
        }
    }

    if !note_blobs.is_empty() {
        let g = w.add_group(0, NOTE_BLOBS_ARGUMENT, 0, count_u32(note_blobs.len())?)?;
        let lengths: Vec<u64> = note_blobs.iter().map(|b| b.len() as u64).collect();
        let mut joined = Vec::with_capacity(note_blobs.iter().map(|b| b.len()).sum());
        for blob in &note_blobs {
            joined.extend_from_slice(blob);
        }
        add_u64_col(&mut w, g, 0, Coding::Plain, NO_KEY_COLUMN, false, &lengths)?;
        w.add_column(g, 1, Coding::Bytes, NO_KEY_COLUMN, 0, &joined)?;
    }

    w.finish(key)
}

#[expect(
    clippy::too_many_lines,
    reason = "encodes syscall stat summary columns and histogram buckets"
)]
pub(super) fn encode_syscall_stats(
    capture: &Capture,
    zstd_level: i32,
    key: u64,
) -> Result<EncodedSection, ObjectError> {
    let mut w = SectionWriter::new(
        SectionKind::SyscallStats,
        TABLE_VERSION,
        TimeUnit::Ns,
        count_u32(capture.syscall_stats.len())?,
        0,
        zstd_level,
    );
    let calls: Vec<u64> = capture
        .syscall_stats
        .iter()
        .map(|r| r.calls.get())
        .collect();
    let errors: Vec<u64> = capture
        .syscall_stats
        .iter()
        .map(|r| r.errors.get())
        .collect();
    let timed: Vec<u64> = capture
        .syscall_stats
        .iter()
        .map(|r| r.timed.get())
        .collect();
    let total_ns: Vec<u64> = capture
        .syscall_stats
        .iter()
        .map(|r| r.total_ns.get())
        .collect();
    let min_ns: Vec<u64> = capture
        .syscall_stats
        .iter()
        .map(|r| r.min_ns.get())
        .collect();
    let max_ns: Vec<u64> = capture
        .syscall_stats
        .iter()
        .map(|r| r.max_ns.get())
        .collect();
    let bytes: Vec<u64> = capture
        .syscall_stats
        .iter()
        .map(|r| r.bytes.get())
        .collect();
    let procs: Vec<u64> = capture
        .syscall_stats
        .iter()
        .map(|r| u64::from(r.process.get()))
        .collect();
    let threads: Vec<u64> = capture
        .syscall_stats
        .iter()
        .map(|r| u64::from(r.thread.get()))
        .collect();
    let nums: Vec<u64> = capture
        .syscall_stats
        .iter()
        .map(|r| u64::from(r.number.get()))
        .collect();
    let ecounts: Vec<u64> = capture
        .syscall_stats
        .iter()
        .map(|r| u64::from(r.errno_count.get()))
        .collect();
    let abis: Vec<u64> = capture
        .syscall_stats
        .iter()
        .map(|r| u64::from(r.abi))
        .collect();

    add_u64_col(
        &mut w,
        WHOLE_SECTION_GROUP,
        0,
        Coding::Plain,
        NO_KEY_COLUMN,
        false,
        &calls,
    )?;
    add_u64_col(
        &mut w,
        WHOLE_SECTION_GROUP,
        1,
        Coding::Plain,
        NO_KEY_COLUMN,
        false,
        &errors,
    )?;
    add_u64_col(
        &mut w,
        WHOLE_SECTION_GROUP,
        2,
        Coding::Plain,
        NO_KEY_COLUMN,
        false,
        &timed,
    )?;
    add_u64_col(
        &mut w,
        WHOLE_SECTION_GROUP,
        3,
        Coding::Plain,
        NO_KEY_COLUMN,
        false,
        &total_ns,
    )?;
    add_u64_col(
        &mut w,
        WHOLE_SECTION_GROUP,
        4,
        Coding::Plain,
        NO_KEY_COLUMN,
        false,
        &min_ns,
    )?;
    add_u64_col(
        &mut w,
        WHOLE_SECTION_GROUP,
        5,
        Coding::Plain,
        NO_KEY_COLUMN,
        false,
        &max_ns,
    )?;
    add_u64_col(
        &mut w,
        WHOLE_SECTION_GROUP,
        6,
        Coding::Plain,
        NO_KEY_COLUMN,
        false,
        &bytes,
    )?;
    add_u64_col(
        &mut w,
        WHOLE_SECTION_GROUP,
        7,
        Coding::Delta,
        NO_KEY_COLUMN,
        false,
        &delta_encode_u64(&procs),
    )?;
    add_u64_col(
        &mut w,
        WHOLE_SECTION_GROUP,
        8,
        Coding::Delta,
        NO_KEY_COLUMN,
        false,
        &delta_encode_u64(&threads),
    )?;
    add_u64_col(
        &mut w,
        WHOLE_SECTION_GROUP,
        9,
        Coding::Plain,
        NO_KEY_COLUMN,
        false,
        &nums,
    )?;
    add_u64_col(
        &mut w,
        WHOLE_SECTION_GROUP,
        10,
        Coding::Plain,
        NO_KEY_COLUMN,
        false,
        &ecounts,
    )?;
    add_u64_col(
        &mut w,
        WHOLE_SECTION_GROUP,
        11,
        Coding::Plain,
        NO_KEY_COLUMN,
        false,
        &abis,
    )?;

    for bucket in 0..crate::syscalls::stats::BUCKETS {
        let vals: Vec<u64> = capture
            .syscall_stats
            .iter()
            .map(|r| r.log2_ns.get(bucket).map_or(0, |v| v.get()))
            .collect();
        if vals.iter().any(|&v| v != 0) {
            let col_id = u8::try_from(12 + bucket).unwrap_or(u8::MAX);
            add_u64_col(
                &mut w,
                WHOLE_SECTION_GROUP,
                col_id,
                Coding::Plain,
                NO_KEY_COLUMN,
                false,
                &vals,
            )?;
        }
    }

    w.finish(key)
}
