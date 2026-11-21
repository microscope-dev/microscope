use std::collections::BTreeMap;

use super::super::codec::{
    Coding, NO_KEY_COLUMN, SectionWriter, WHOLE_SECTION_GROUP, delta_by_key_encode_u64,
    delta_encode_i64, encode_dict_u64, encode_sparse_histograms, encode_words, zigzag_encode,
};
use super::super::{DEFAULT_PAGE_ROWS, EncodedSection, ObjectError, SectionKind, TimeUnit};
use super::{
    NOTE_BLOBS_ARGUMENT, TABLE_VERSION, add_delta_u64_col, add_plain_u64_col, add_u64_col,
    count_u32, remap_stack, syscall_arg_count,
};
use crate::capture::Capture;
use crate::capture::records::{Syscall, UNSEEN, captures as parse_captures};
use crate::syscalls::{Capture as ArgCapture, CaptureState, ReadAt};

/// Encodes `Syscalls` paged by time (`DEFAULT_PAGE_ROWS`), with two column-group frames per page:
/// - Frame 0: core timing/thread/result/stack/outcome columns
/// - Frame 1: syscall argument columns (`args[0..arg_count]`), skippable by list views
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

            let raw_t = if call.exited.get() != UNSEEN {
                Some(call.exited.get())
            } else if call.entered.get() != UNSEEN {
                Some(call.entered.get())
            } else {
                None
            };
            let t = raw_t.map_or(prev_exit, |rt| {
                min_t = min_t.min(rt);
                max_t = max_t.max(rt);
                time_unit.to_units(rt)
            });
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

        // Frame 0: core columns (order, exit_times, duration, thread, ret, stack, cap_len, outcome, flags, unseen).
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

        // Frame 1: syscall arguments (separate zstd frame when section is large enough to
        // benefit from skipping `args`, or same frame for tiny sections to avoid frame overhead).
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
    add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 0, &call_counts)?;
    add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &order)?;

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
            Coding::DictVarint,
            NO_KEY_COLUMN,
            &encode_dict_u64(&headers),
        )?;
        add_plain_u64_col(&mut w, g, 1, &wanted)?;
        add_plain_u64_col(&mut w, g, 2, &lengths)?;

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

    if !note_blobs.is_empty() {
        let g = w.add_group(0, NOTE_BLOBS_ARGUMENT, 0, count_u32(note_blobs.len())?)?;
        let lengths: Vec<u64> = note_blobs.iter().map(|b| b.len() as u64).collect();
        let mut joined = Vec::with_capacity(note_blobs.iter().map(|b| b.len()).sum());
        for blob in &note_blobs {
            joined.extend_from_slice(blob);
        }
        add_plain_u64_col(&mut w, g, 0, &lengths)?;
        w.add_column(g, 1, Coding::Bytes, NO_KEY_COLUMN, &joined)?;
    }

    w.finish(key)
}

/// Encodes `SyscallStats` sorted by `(abi, number, process, thread)` using sparse 40-bit
/// histograms, and reorders `SyscallErrnos` in lockstep so `errnos..errnos + errno_count`
/// remains exact.
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
                // Fallback if errnos exist without stat ranges pointing into them.
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
