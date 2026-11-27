use std::collections::BTreeMap;

use super::super::codec::{
    Coding, NO_KEY_COLUMN, SectionWriter, WHOLE_SECTION_GROUP, delta_by_key_encode_u64,
    delta_encode_i64, encode_dict_u64, encode_sparse_histograms, encode_words, zigzag_encode,
};
use super::super::{DEFAULT_PAGE_ROWS, EncodedSection, ObjectError, SectionKind, TimeUnit};
use super::{
    NOTE_BLOBS_ARGUMENT, TABLE_VERSION, add_delta_u64_col, add_plain_u64_col, add_u64_col,
    count_u32, remap_stack,
};
use crate::capture::Capture;
use crate::capture::records::{Syscall, UNSEEN, captures as parse_captures};
use crate::syscalls::{Capture as ArgCapture, CaptureState, ReadAt};

/// Encodes syscalls grouped per page by `(abi, number)` for tighter per-syscall
/// delta compression, preserving chronology in an `order` column.
///
/// Pages with at least 1024 rows split syscall arguments into a second frame so
/// summary scans can skip decompressing arguments.
#[expect(
    clippy::too_many_lines,
    reason = "encodes paged grouped syscall columns with core vs args framing"
)]
pub(super) fn encode_syscalls(
    capture: &Capture,
    old_to_dfs: &[u32],
    time_unit: TimeUnit,
    zstd_level: i32,
    key: u64,
) -> Result<EncodedSection, ObjectError> {
    let total = capture.syscalls.len();
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

    let mut w = SectionWriter::new(
        SectionKind::Syscalls,
        TABLE_VERSION,
        time_unit,
        count_u32(total)?,
        first_time,
        zstd_level,
    );

    let mut page_start = 0_usize;
    while page_start < total {
        let page_end = (page_start + DEFAULT_PAGE_ROWS).min(total);
        let page_rows = &capture.syscalls[page_start..page_end];

        let mut by_kind: BTreeMap<(u8, u16), Vec<&Syscall>> = BTreeMap::new();
        let mut keys_per_row = Vec::with_capacity(page_rows.len());
        let mut min_t = i64::MAX;
        let mut max_t = i64::MIN;
        let mut exit_times = Vec::with_capacity(page_rows.len());
        let mut prev_exit = first_time;

        for call in page_rows {
            let nr = u16::try_from(call.number.get())
                .map_err(|_| ObjectError::Layout("syscall number exceeds u16".into()))?;
            let gk = (call.abi, nr);
            keys_per_row.push(gk);
            by_kind.entry(gk).or_default().push(call);

            // Substitute `entered` (or `prev_exit`) when `exited == UNSEEN` so delta
            // coding stays near zero; a 2-bit `unseen` mask column restores `UNSEEN`.
            let raw_t = if call.exited.get() != UNSEEN {
                Some(call.exited.get())
            } else if call.entered.get() != UNSEEN {
                Some(call.entered.get())
            } else {
                None
            };
            let t = raw_t.map_or(prev_exit, |rt| {
                let u = time_unit.to_units(rt);
                let decoded_t = time_unit.to_nanos(u);
                min_t = min_t.min(decoded_t);
                max_t = max_t.max(decoded_t);
                u
            });
            if call.entered.get() != UNSEEN {
                let decoded_entered = time_unit.to_nanos(time_unit.to_units(call.entered.get()));
                min_t = min_t.min(decoded_entered);
                max_t = max_t.max(decoded_entered);
            }
            exit_times.push(t);
            prev_exit = t;
        }
        if min_t > max_t {
            min_t = 0;
            max_t = 0;
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

        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 0, &order)?;
        let exit_deltas = delta_encode_i64(&exit_times, first_time);
        if time_unit == TimeUnit::Ns {
            w.add_binned_2b(WHOLE_SECTION_GROUP, 1, &exit_deltas)?;
        } else {
            add_u64_col(
                &mut w,
                WHOLE_SECTION_GROUP,
                1,
                Coding::VarintPlain,
                NO_KEY_COLUMN,
                &exit_deltas,
            )?;
        }

        let mut pending_args: Vec<(u16, Vec<u32>, Vec<Vec<u64>>)> =
            Vec::with_capacity(by_kind.len());

        for ((abi, nr), rows) in by_kind {
            let g = w.add_group(abi, u8::MAX, nr, count_u32(rows.len())?)?;
            let arg_count = rows
                .iter()
                .filter_map(|call| {
                    call.args
                        .iter()
                        .enumerate()
                        .rev()
                        .find(|(_, v)| v.get() != 0)
                        .map(|(idx, _)| idx + 1)
                })
                .max()
                .unwrap_or(0);

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
                stacks.push(u64::from(remap_stack(call.stack.get(), old_to_dfs)));
                cap_lens.push(u64::from(call.captures_len.get()));
                outcomes.push(u64::from(call.outcome));
                flags.push(u64::from(call.flags));
                for (i, col) in args.iter_mut().enumerate() {
                    if let Some(arg) = call.args.get(i) {
                        col.push(arg.get());
                    }
                }
            }

            if time_unit == TimeUnit::Ns {
                w.add_binned_2b(g, 0, &durations)?;
            } else {
                add_plain_u64_col(&mut w, g, 0, &durations)?;
            }
            add_plain_u64_col(&mut w, g, 1, &threads)?;
            add_u64_col(
                &mut w,
                g,
                2,
                Coding::VarintDeltaByKey,
                1,
                &delta_by_key_encode_u64(&rets, &threads_u32),
            )?;
            add_u64_col(
                &mut w,
                g,
                3,
                Coding::VarintDeltaByKey,
                1,
                &delta_by_key_encode_u64(&stacks, &threads_u32),
            )?;
            add_plain_u64_col(&mut w, g, 4, &cap_lens)?;
            add_plain_u64_col(&mut w, g, 5, &outcomes)?;
            add_plain_u64_col(&mut w, g, 6, &flags)?;
            if any_unseen {
                add_plain_u64_col(&mut w, g, 7, &unseen)?;
            }

            if !args.is_empty() {
                pending_args.push((g, threads_u32, args));
            }
        }

        let split_args_frame = page_rows.len() >= 1024 && !pending_args.is_empty();
        if split_args_frame {
            w.next_frame()?;
        }
        for (g, threads_u32, args) in pending_args {
            for (i, col) in args.iter().enumerate() {
                let col_id = u8::try_from(10 + i).unwrap_or(15);
                add_u64_col(
                    &mut w,
                    g,
                    col_id,
                    Coding::VarintDeltaByKey,
                    1,
                    &delta_by_key_encode_u64(col, &threads_u32),
                )?;
            }
        }

        w.finish_page(min_t, max_t, count_u32(page_rows.len())?)?;
        page_start = page_end;
    }

    w.finish_paged(key)
}

const SYSCALL_BLOBS_PAGE_BYTES: usize = 16 * 1024 * 1024;

fn flush_syscall_blob_page(
    w: &mut SectionWriter,
    call_start: usize,
    call_end: usize,
    call_counts: &mut Vec<u64>,
    call_arg_order: &mut Vec<(u8, u16, u8)>,
    by_arg: &mut BTreeMap<(u8, u16, u8), Vec<ArgCapture>>,
    page_notes: &mut Vec<&[u8]>,
) -> Result<(), ObjectError> {
    let group_index: BTreeMap<(u8, u16, u8), u64> = by_arg
        .keys()
        .enumerate()
        .map(|(idx, &gk)| (gk, idx as u64))
        .collect();
    let order: Vec<u64> = call_arg_order
        .iter()
        .map(|gk| group_index.get(gk).copied().unwrap_or(0))
        .collect();
    let page_captures = call_arg_order.len();

    add_plain_u64_col(w, WHOLE_SECTION_GROUP, 0, call_counts)?;
    add_plain_u64_col(w, WHOLE_SECTION_GROUP, 1, &order)?;

    for ((abi, nr, arg), caps) in by_arg.iter() {
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
            Coding::DictVarint,
            NO_KEY_COLUMN,
            &encode_dict_u64(&headers),
        )?;
        add_plain_u64_col(w, g, 1, &wanted)?;
        add_plain_u64_col(w, g, 2, &lengths)?;

        let first_len = caps.first().map_or(0, |c| c.bytes.len());
        let fixed_words = first_len > 0
            && first_len.is_multiple_of(8)
            && caps.iter().all(|c| c.bytes.len() == first_len);
        if fixed_words {
            let refs: Vec<&[u8]> = caps.iter().map(|c| c.bytes.as_ref()).collect();
            let transposed = encode_words(&refs, first_len);
            w.add_column(g, 3, Coding::Words, NO_KEY_COLUMN, &transposed)?;
        } else {
            let mut joined = Vec::with_capacity(caps.iter().map(|c| c.bytes.len()).sum());
            for cap in caps {
                joined.extend_from_slice(&cap.bytes);
            }
            w.add_column(g, 3, Coding::Bytes, NO_KEY_COLUMN, &joined)?;
        }
    }

    if !page_notes.is_empty() {
        let g = w.add_group(0, NOTE_BLOBS_ARGUMENT, 0, count_u32(page_notes.len())?)?;
        let lengths: Vec<u64> = page_notes.iter().map(|b| b.len() as u64).collect();
        let mut joined = Vec::with_capacity(page_notes.iter().map(|b| b.len()).sum());
        for blob in page_notes.iter() {
            joined.extend_from_slice(blob);
        }
        add_plain_u64_col(w, g, 0, &lengths)?;
        w.add_column(g, 1, Coding::Bytes, NO_KEY_COLUMN, &joined)?;
    }

    let min_k = i64::try_from(call_start)
        .map_err(|_| ObjectError::Limit("syscall call index exceeds i64"))?;
    let max_k = i64::try_from(call_end)
        .map_err(|_| ObjectError::Limit("syscall call index exceeds i64"))?;
    w.finish_page(min_k, max_k, count_u32(page_captures)?)?;

    call_counts.clear();
    call_arg_order.clear();
    by_arg.clear();
    page_notes.clear();
    Ok(())
}

/// Encodes per-argument capture blobs alongside `SyscallNote` blobs,
/// multiplexing note blobs into the same section under `NOTE_BLOBS_ARGUMENT`
/// (`u8::MAX`).
#[expect(
    clippy::too_many_lines,
    reason = "encodes paged grouped syscall capture blobs and note blobs"
)]
pub(super) fn encode_syscall_blobs(
    capture: &Capture,
    zstd_level: i32,
    key: u64,
) -> Result<EncodedSection, ObjectError> {
    let mut parsed_calls: Vec<(u8, u16, Vec<ArgCapture>, usize)> =
        Vec::with_capacity(capture.syscalls.len());
    let mut total_captures = 0_usize;

    for call in &capture.syscalls {
        let bytes = capture.blob(call.captures.get(), call.captures_len.get());
        if bytes.is_empty() {
            parsed_calls.push((call.abi, 0, Vec::new(), 10));
            continue;
        }
        let parsed = parse_captures(bytes)
            .ok_or_else(|| ObjectError::Capture("malformed syscall capture bytes".into()))?;
        let nr = u16::try_from(call.number.get())
            .map_err(|_| ObjectError::Layout("syscall number exceeds u16".into()))?;
        let est_bytes: usize = parsed
            .iter()
            .map(|cap| cap.bytes.len().saturating_add(48))
            .sum::<usize>()
            .saturating_add(16);
        total_captures = total_captures.saturating_add(parsed.len());
        parsed_calls.push((call.abi, nr, parsed, est_bytes));
    }

    let mut note_blobs: Vec<&[u8]> = Vec::new();
    for note in &capture.syscall_notes {
        let bytes = capture.blob(note.blob.get(), note.blob_len.get());
        if !bytes.is_empty() {
            note_blobs.push(bytes);
        }
    }

    let mut w = SectionWriter::new(
        SectionKind::SyscallBlobs,
        TABLE_VERSION,
        TimeUnit::Ns,
        count_u32(total_captures)?,
        0,
        zstd_level,
    );

    let mut call_counts = Vec::new();
    let mut call_arg_order = Vec::new();
    let mut by_arg: BTreeMap<(u8, u16, u8), Vec<ArgCapture>> = BTreeMap::new();
    let mut page_notes: Vec<&[u8]> = Vec::new();
    let mut page_bytes = 0_usize;
    let mut page_call_start = 0_usize;
    let mut emitted_any = false;

    for (call_idx, (abi, nr, parsed, est_bytes)) in parsed_calls.into_iter().enumerate() {
        if !call_counts.is_empty()
            && (page_bytes.saturating_add(est_bytes) > SYSCALL_BLOBS_PAGE_BYTES
                || call_counts.len() >= DEFAULT_PAGE_ROWS)
        {
            flush_syscall_blob_page(
                &mut w,
                page_call_start,
                call_idx,
                &mut call_counts,
                &mut call_arg_order,
                &mut by_arg,
                &mut page_notes,
            )?;
            page_call_start = call_idx;
            page_bytes = 0;
            emitted_any = true;
        }
        call_counts.push(parsed.len() as u64);
        for cap in parsed {
            let gk = (abi, nr, cap.arg);
            call_arg_order.push(gk);
            by_arg.entry(gk).or_default().push(cap);
        }
        page_bytes = page_bytes.saturating_add(est_bytes);
    }

    let total_calls = capture.syscalls.len();
    for blob in note_blobs {
        let est_bytes = blob.len().saturating_add(16);
        if (!call_counts.is_empty() || !page_notes.is_empty())
            && page_bytes.saturating_add(est_bytes) > SYSCALL_BLOBS_PAGE_BYTES
        {
            flush_syscall_blob_page(
                &mut w,
                page_call_start,
                total_calls,
                &mut call_counts,
                &mut call_arg_order,
                &mut by_arg,
                &mut page_notes,
            )?;
            page_call_start = total_calls;
            page_bytes = 0;
            emitted_any = true;
        }
        page_notes.push(blob);
        page_bytes = page_bytes.saturating_add(est_bytes);
    }

    if !call_counts.is_empty() || !page_notes.is_empty() || !emitted_any {
        flush_syscall_blob_page(
            &mut w,
            page_call_start,
            total_calls,
            &mut call_counts,
            &mut call_arg_order,
            &mut by_arg,
            &mut page_notes,
        )?;
    }

    w.finish_paged(key)
}

#[expect(
    clippy::too_many_lines,
    reason = "encodes sorted sparse syscall stats and paired errnos together"
)]
pub(super) fn encode_syscall_stats_and_errnos(
    capture: &Capture,
    zstd_level: i32,
    key: u64,
) -> Result<(Option<EncodedSection>, Option<EncodedSection>), ObjectError> {
    let stats_section = if capture.syscall_stats.is_empty() {
        None
    } else {
        let mut order: Vec<usize> = (0..capture.syscall_stats.len()).collect();
        order.sort_by_key(|&i| {
            let r = &capture.syscall_stats[i];
            (r.abi, r.number.get(), r.process.get(), r.thread.get())
        });

        let mut w = SectionWriter::new(
            SectionKind::SyscallStats,
            TABLE_VERSION,
            TimeUnit::Ns,
            count_u32(order.len())?,
            0,
            zstd_level,
        );

        let mut calls = Vec::with_capacity(order.len());
        let mut errors = Vec::with_capacity(order.len());
        let mut timed = Vec::with_capacity(order.len());
        let mut total_ns = Vec::with_capacity(order.len());
        let mut min_ns = Vec::with_capacity(order.len());
        let mut max_ns = Vec::with_capacity(order.len());
        let mut bytes = Vec::with_capacity(order.len());
        let mut procs = Vec::with_capacity(order.len());
        let mut threads = Vec::with_capacity(order.len());
        let mut nums = Vec::with_capacity(order.len());
        let mut ecounts = Vec::with_capacity(order.len());
        let mut abis = Vec::with_capacity(order.len());
        let mut hists = Vec::with_capacity(order.len());

        for &idx in &order {
            let r = &capture.syscall_stats[idx];
            calls.push(r.calls.get());
            errors.push(r.errors.get());
            timed.push(r.timed.get());
            total_ns.push(r.total_ns.get());
            min_ns.push(r.min_ns.get());
            max_ns.push(r.max_ns.get());
            bytes.push(r.bytes.get());
            procs.push(u64::from(r.process.get()));
            threads.push(u64::from(r.thread.get()));
            nums.push(u64::from(r.number.get()));
            ecounts.push(u64::from(r.errno_count.get()));
            abis.push(u64::from(r.abi));
            let mut h = [0_u64; crate::syscalls::stats::BUCKETS];
            for (slot, val) in h.iter_mut().zip(&r.log2_ns) {
                *slot = val.get();
            }
            hists.push(h);
        }

        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 0, &calls)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &errors)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 2, &timed)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 3, &total_ns)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 4, &min_ns)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 5, &max_ns)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 6, &bytes)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 7, &procs)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 8, &threads)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 9, &nums)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 10, &ecounts)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 11, &abis)?;
        w.add_column(
            WHOLE_SECTION_GROUP,
            12,
            Coding::SparseHistogram40,
            NO_KEY_COLUMN,
            &encode_sparse_histograms(&hists),
        )?;

        let stats_sec = w.finish(key)?;

        let errnos_sec = if capture.syscall_errnos.is_empty() {
            None
        } else {
            let mut reordered_counts = Vec::with_capacity(capture.syscall_errnos.len());
            let mut reordered_errnos = Vec::with_capacity(capture.syscall_errnos.len());
            for &idx in &order {
                let r = &capture.syscall_stats[idx];
                let start = r.errnos.get() as usize;
                let count = r.errno_count.get() as usize;
                if let Some(slice) = capture
                    .syscall_errnos
                    .get(start..start.saturating_add(count))
                {
                    for e in slice {
                        reordered_counts.push(e.count.get());
                        reordered_errnos.push(u64::from(e.errno.get()));
                    }
                }
            }
            if reordered_counts.is_empty() {
                for e in &capture.syscall_errnos {
                    reordered_counts.push(e.count.get());
                    reordered_errnos.push(u64::from(e.errno.get()));
                }
            }
            let mut ew = SectionWriter::new(
                SectionKind::SyscallErrnos,
                TABLE_VERSION,
                TimeUnit::Ns,
                count_u32(reordered_counts.len())?,
                0,
                zstd_level,
            );
            add_plain_u64_col(&mut ew, WHOLE_SECTION_GROUP, 0, &reordered_counts)?;
            add_plain_u64_col(&mut ew, WHOLE_SECTION_GROUP, 1, &reordered_errnos)?;
            Some(ew.finish(key)?)
        };

        return Ok((Some(stats_sec), errnos_sec));
    };

    let errnos_sec = if capture.syscall_errnos.is_empty() {
        None
    } else {
        let mut ew = SectionWriter::new(
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
        add_plain_u64_col(&mut ew, WHOLE_SECTION_GROUP, 0, &counts)?;
        add_plain_u64_col(&mut ew, WHOLE_SECTION_GROUP, 1, &errnos)?;
        Some(ew.finish(key)?)
    };

    Ok((stats_section, errnos_sec))
}
