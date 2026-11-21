use super::super::codec::{
    Coding, DecodeBudget, SectionReader, WHOLE_SECTION_GROUP, decode_sparse_histograms,
    decode_words, delta_by_key_decode_u64, delta_decode_i64, zigzag_decode,
};
use super::super::{MAX_TOTAL_ROWS, ObjectError, SectionKind};
use super::decode::charge_rows;
use super::{
    NOTE_BLOBS_ARGUMENT, read_delta_u64_col, read_u8_col, read_u32_col, read_u64_col, u32_from_u64,
};
use crate::capture::Capture;
use crate::capture::records::{NONE, Syscall, SyscallStat, UNSEEN};

#[expect(
    clippy::too_many_lines,
    reason = "decodes paged grouped syscall columns per abi and number"
)]
pub(super) fn decode_syscalls(
    bytes: &[u8],
    budget: &DecodeBudget,
) -> Result<Vec<Syscall>, ObjectError> {
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
pub(super) fn decode_syscall_blobs(
    bytes: &[u8],
    capture: &mut Capture,
    budget: &DecodeBudget,
) -> Result<(), ObjectError> {
    struct DecodedBlobGroup {
        abi: u8,
        number: u16,
        argument: u8,
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

    let mut blobs = Vec::new();
    let mut call_cursor = 0_usize;
    let mut note_cursor = 0_usize;
    let paged = r.page_count() > 1;

    for page_idx in 0..r.page_count() {
        let (expected_calls, page_captures) = if paged {
            let entry = r
                .page_entry(page_idx)
                .ok_or_else(|| ObjectError::Layout("missing syscall_blobs page entry".into()))?;
            let start = usize::try_from(entry.min_key.get())
                .map_err(|_| ObjectError::Layout("negative syscall_blobs call start".into()))?;
            let end = usize::try_from(entry.max_key.get())
                .map_err(|_| ObjectError::Layout("negative syscall_blobs call end".into()))?;
            if start != call_cursor || end < start || end > capture.syscalls.len() {
                return Err(ObjectError::Layout(
                    "syscall_blobs page call range does not tile syscalls".into(),
                ));
            }
            (end - start, entry.rows.get() as usize)
        } else {
            (capture.syscalls.len(), total_captures)
        };

        let p = r.read_page(page_idx)?;
        let before_cols = budget.remaining();
        let call_counts = read_u32_col(&p, WHOLE_SECTION_GROUP, 0, expected_calls)?;
        let order = read_u64_col(&p, WHOLE_SECTION_GROUP, 1, page_captures)?;

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
                abi: entry.abi,
                number: entry.number.get(),
                argument: entry.argument,
                headers,
                wanted,
                lengths,
                data,
                data_offset: 0,
                cursor: 0,
            }));
        }

        let mut order_pos = 0_usize;
        let mut retained_bytes = 0_usize;
        let call_slice = capture
            .syscalls
            .get_mut(call_cursor..call_cursor + expected_calls)
            .ok_or_else(|| ObjectError::Layout("syscall_blobs call slice out of bounds".into()))?;
        call_cursor += expected_calls;

        for (call, &cap_count) in call_slice.iter_mut().zip(&call_counts) {
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
                let group = groups.get_mut(gi).and_then(Option::as_mut).ok_or_else(|| {
                    ObjectError::Layout("blob order refers to missing group".into())
                })?;
                let pos = group.cursor;
                if pos >= group.headers.len() {
                    return Err(ObjectError::Layout(
                        "blob order exceeds group row count".into(),
                    ));
                }
                group.cursor += 1;

                let hdr = group.headers.get(pos).copied().unwrap_or(0);
                let arg = (hdr & 0xff) as u8;
                if group.abi != call.abi
                    || u32::from(group.number) != call.number.get()
                    || group.argument != arg
                {
                    return Err(ObjectError::Layout(
                        "syscall blob group metadata disagrees with owning syscall".into(),
                    ));
                }
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
                retained_bytes = retained_bytes.saturating_add(payload.len());
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
            while note_cursor < capture.syscall_notes.len() && idx < lengths.len() {
                let note = &mut capture.syscall_notes[note_cursor];
                note_cursor += 1;
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
                retained_bytes = retained_bytes.saturating_add(slice.len());
                blobs.extend_from_slice(slice);
                note.blob = start.into();
                note.blob_len = len.into();
            }
            if idx != lengths.len() || off != data.len() {
                return Err(ObjectError::Layout("unconsumed note blob data".into()));
            }
        }

        let page_charged = before_cols.saturating_sub(budget.remaining());
        budget.release(page_charged.saturating_sub(retained_bytes));
    }

    if call_cursor != capture.syscalls.len() {
        return Err(ObjectError::Layout(
            "syscall_blobs did not cover all syscalls".into(),
        ));
    }
    for note in capture.syscall_notes.iter_mut().skip(note_cursor) {
        if note.blob_len.get() == 0 {
            note.blob = NONE.into();
        } else {
            return Err(ObjectError::Layout("missing note blob data".into()));
        }
    }

    capture.syscall_blobs = blobs;
    Ok(())
}

pub(super) fn decode_syscall_stats(
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
