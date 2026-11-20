use proptest::prelude::*;

use crate::capture::CaptureBuilder;
use crate::capture::builder::LineInput;
use crate::capture::records::{
    AccountKind, CallOutcome, FrameKind, GapReason, Mapping, MarkerKind, NONE, NoteKind,
    OFF_CPU_BEGAN_UNSEEN, Syscall, SyscallErrno, SyscallNote, SyscallStat, UNSEEN, Zone,
};
use crate::model::{Pid, Tid};
use crate::sanitize::chunk::encode_post;
use crate::sanitize::{Builtin, FieldClass, HashKey, Sanitizer, sanitize_chunk};

use super::codec::{
    Coding, SectionWriter, WHOLE_SECTION_GROUP, decode_bitpacked, decode_dict_u64, decode_varints,
    decode_words, delta_by_key_decode_u64, delta_by_key_encode_u64, delta_decode_i64,
    delta_decode_u64, delta_encode_i64, delta_encode_u64, encode_bitpacked, encode_dict_u64,
    encode_varints, encode_words, relative_decode_i64, relative_encode_i64, zigzag_decode,
    zigzag_encode,
};
use super::{
    ChunkData, ChunkRow, DEFAULT_ZSTD_LEVEL, DimensionRow, ObjectError, ObjectKind, ObjectLevel,
    ObjectReader, ObjectWriter, PartKind, PartSpec, SanitizerCountRow, SectionKind, Signal,
    TimeUnit, decode_flame_tables, decode_tables, encode_tables, merge_chunks, round_capture_times,
    shift_capture_times,
};

#[expect(
    clippy::too_many_lines,
    reason = "fixture populates every capture table"
)]
fn build_rich_capture() -> crate::capture::Capture {
    let mut b = CaptureBuilder::new();
    b.setting("start", "1000000000");
    b.setting("end", "1005000000");
    b.setting("arguments", "/usr/bin/demo --token=supersecret123");
    b.setting("weight", "nanoseconds");

    let module = b.module("/usr/bin/demo", Some("deadbeef"), [7; 16], 4096);
    let proc_id = b.process(Pid::new(42), 1_000_000_123);
    b.process_name(proc_id, "demo");
    let t1 = b.thread(Pid::new(42), Tid::new(42), 1_000_000_123);
    b.thread_name(t1, "main");
    let t2 = b.thread(Pid::new(42), Tid::new(43), 1_000_001_456);
    b.thread_name(t2, "worker");

    b.mapping(Mapping {
        process: proc_id.into(),
        module: module.into(),
        start: 0x40_0000.into(),
        end: 0x40_4000.into(),
        file_offset: 0.into(),
        bias: 0x40_0000.into(),
        time: 1_000_000_123.into(),
    });

    let f1 = b.frame(FrameKind::Native, module, 0x100, 0);
    let f2 = b.frame(FrameKind::Native, module, 0x200, 0);
    b.symbolize(
        f1,
        module,
        &[LineInput {
            function: "hot_loop",
            file: Some("/home/alice/src/demo.c"),
            line: 12,
            column: 4,
            declared: Some(("/home/alice/src/demo.c", 10)),
            role: 0,
        }],
    );
    b.symbolize(
        f2,
        module,
        &[LineInput {
            function: "main",
            file: Some("/home/alice/src/demo.c"),
            line: 24,
            column: 1,
            declared: None,
            role: 0,
        }],
    );
    let stack = b.stack(&[f1, f2]);
    let task = b.task("tokio", 99, 1_000_002_000);
    b.describe_task(task, f2, Some("req=1"));

    b.sample(t1, 1_000_005_321, 20_000_000, stack, task);
    b.sample(t1, 1_000_025_789, 20_000_000, stack, NONE);
    b.sample(t2, 1_000_015_111, 20_000_000, stack, NONE);

    b.off_cpu(t1, 1_000_030_000, 5_000_432, true, OFF_CPU_BEGAN_UNSEEN);
    b.off_cpu(t1, 1_000_040_000, 2_500_000, false, 0);

    let site = b.site("handle_request", "/home/alice/src/demo.c", 30, module, 1);
    let zone_val = b.string("https://user:hunter2@db.internal/orders");
    b.zone(
        t1,
        Zone {
            start: 1_000_006_000.into(),
            end: 1_000_020_000.into(),
            site: site.into(),
            flags: 0.into(),
            value: zone_val.into(),
            reserved: 0.into(),
        },
    );
    b.plot(t1, site, 1_000_010_000, 42.5);
    b.thread_marker(
        (t1, site),
        1_000_011_000,
        MarkerKind::Message,
        Some("Bearer tok_secret_abc1234567890"),
    );
    b.task_wait(task, 1_000_012_000, 1_000_000, stack, "poll");
    b.gap(1_000_050_000, 1_000_055_000, GapReason::Behind);
    b.count(AccountKind::KeptSamples as u32, 3);

    let mut read_blob = Vec::new();
    let payload = b"AKIAIOSFODNN7EXAMPLE secret payload";
    read_blob.extend_from_slice(&[1, 1, 0, 0]);
    read_blob.extend_from_slice(&0_u32.to_le_bytes());
    read_blob.extend_from_slice(&u32::try_from(payload.len()).expect("len").to_le_bytes());
    read_blob.extend_from_slice(payload);

    b.syscall(
        Syscall {
            entered: 1_000_006_100.into(),
            exited: 1_000_006_950.into(),
            ret: i64::try_from(payload.len()).expect("len").into(),
            args: [
                3.into(),
                0x7fff_0000_1000.into(),
                4096.into(),
                0.into(),
                0.into(),
                0.into(),
            ],
            thread: t1.into(),
            number: 0.into(),
            stack: stack.into(),
            captures: NONE.into(),
            captures_len: 0.into(),
            abi: 0,
            outcome: CallOutcome::Returned as u8,
            flags: 0,
            reserved: 0,
        },
        &read_blob,
    );

    let mut ts_blob = Vec::new();
    let timespec = [1_u8, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0];
    ts_blob.extend_from_slice(&[0, 0, 0, 0]);
    ts_blob.extend_from_slice(&0_u32.to_le_bytes());
    ts_blob.extend_from_slice(&u32::try_from(timespec.len()).expect("len").to_le_bytes());
    ts_blob.extend_from_slice(&timespec);

    b.syscall(
        Syscall {
            entered: UNSEEN.into(),
            exited: 1_000_008_200.into(),
            ret: 0.into(),
            args: [
                0x7fff_0000_2000.into(),
                0.into(),
                0.into(),
                0.into(),
                0.into(),
                0.into(),
            ],
            thread: t2.into(),
            number: 35.into(),
            stack: stack.into(),
            captures: NONE.into(),
            captures_len: 0.into(),
            abi: 0,
            outcome: CallOutcome::Returned as u8,
            flags: 1,
            reserved: 0,
        },
        &ts_blob,
    );

    let note_text = b.string("/tmp/secret?password=my_db_password");
    b.syscall_note(
        SyscallNote {
            time: 1_000_009_000.into(),
            value: 3.into(),
            extra: 0.into(),
            thread: t1.into(),
            process: proc_id.into(),
            text: note_text.into(),
            stack: stack.into(),
            blob: NONE.into(),
            blob_len: 0.into(),
            calls: 0.into(),
            kind: NoteKind::Descriptor as u8,
            flags: 0,
            reserved: [0; 10],
        },
        b"note-bytes",
    );

    let mut log2_ns = [zerocopy::little_endian::U64::new(0); crate::syscalls::stats::BUCKETS];
    log2_ns[10] = 5.into();
    b.syscall_stat(
        SyscallStat {
            calls: 5.into(),
            errors: 1.into(),
            timed: 5.into(),
            total_ns: 4500.into(),
            min_ns: 500.into(),
            max_ns: 1500.into(),
            bytes: 128.into(),
            log2_ns,
            process: proc_id.into(),
            thread: t1.into(),
            number: 0.into(),
            errnos: 0.into(),
            errno_count: 0.into(),
            abi: 0,
            reserved: [0; 3],
        },
        &[SyscallErrno {
            count: 1.into(),
            errno: 11.into(),
            reserved: [0; 6],
        }],
    );

    b.finish()
}

#[test]
fn rich_capture_round_trips_losslessly_at_1ns() {
    let capture = build_rich_capture();
    let data = ChunkData {
        capture: capture.clone(),
        chunks: vec![ChunkRow {
            agent: 77,
            number: 1,
            start: 1_000_000_000,
            end: 1_005_000_000,
            sampled_ns: 4_800_000_000,
            preset: 0,
            time_unit: TimeUnit::Ns,
            policy: 1,
            rules: "secrets in arguments".into(),
        }],
        dimensions: vec![
            DimensionRow {
                process: 0,
                name: "service.name".into(),
                value: "demo-svc".into(),
            },
            DimensionRow {
                process: 0,
                name: "microscope.project".into(),
                value: "demo-proj".into(),
            },
        ],
        sanitizer_counts: vec![SanitizerCountRow {
            rule: "token shapes".into(),
            field: FieldClass::SyscallPayload.number(),
            fired: 2,
        }],
    };

    let sections = encode_tables(&data, TimeUnit::Ns, DEFAULT_ZSTD_LEVEL, 99).expect("encode");
    let mut writer = ObjectWriter::new(ObjectKind::Post, ObjectLevel::None, 1_005_000_000, 77);
    writer
        .add_part(&PartSpec {
            org_id: 42,
            project_id: 7,
            starts_at: 1_000_000_000,
            ends_at: 1_005_000_000,
            signal: Signal::Profile,
            part: PartKind::Chunks,
            sections,
        })
        .expect("add part");
    let (bytes, metas) = writer.finish().expect("finish");
    assert_eq!(metas.len(), 1);
    assert_eq!(metas[0].org_id, 42);

    let reader = ObjectReader::open(&bytes).expect("open");
    assert_eq!(reader.kind, ObjectKind::Post);
    assert_eq!(reader.writer, 77);
    let raw_sections = reader.part_sections(0, Some(42)).expect("part sections");
    let decoded = decode_tables(&raw_sections).expect("decode");

    assert_eq!(decoded.chunks, data.chunks);
    assert_eq!(decoded.dimensions, data.dimensions);
    assert_eq!(decoded.sanitizer_counts, data.sanitizer_counts);
    assert_eq!(decoded.capture.processes, capture.processes);
    assert_eq!(decoded.capture.threads, capture.threads);
    assert_eq!(decoded.capture.modules, capture.modules);
    assert_eq!(decoded.capture.mappings, capture.mappings);
    assert_eq!(decoded.capture.frames, capture.frames);
    assert_eq!(decoded.capture.stacks, capture.stacks);
    assert_eq!(decoded.capture.samples, capture.samples);
    assert_eq!(decoded.capture.sample_ranges, capture.sample_ranges);
    assert_eq!(decoded.capture.frame_symbols, capture.frame_symbols);
    assert_eq!(decoded.capture.lines, capture.lines);
    assert_eq!(decoded.capture.functions, capture.functions);
    assert_eq!(decoded.capture.accounting, capture.accounting);
    assert_eq!(decoded.capture.off_cpu, capture.off_cpu);
    assert_eq!(decoded.capture.off_cpu_ranges, capture.off_cpu_ranges);
    assert_eq!(decoded.capture.sites, capture.sites);
    assert_eq!(decoded.capture.zones, capture.zones);
    assert_eq!(decoded.capture.zone_ranges, capture.zone_ranges);
    assert_eq!(decoded.capture.plots, capture.plots);
    assert_eq!(decoded.capture.markers, capture.markers);
    assert_eq!(decoded.capture.tasks, capture.tasks);
    assert_eq!(decoded.capture.task_waits, capture.task_waits);
    assert_eq!(decoded.capture.gaps, capture.gaps);
    assert_eq!(decoded.capture.syscalls, capture.syscalls);
    assert_eq!(decoded.capture.syscall_blobs, capture.syscall_blobs);
    assert_eq!(decoded.capture.syscall_notes, capture.syscall_notes);
    assert_eq!(decoded.capture.syscall_stats, capture.syscall_stats);
    assert_eq!(decoded.capture.syscall_errnos, capture.syscall_errnos);

    let flame_only = decode_flame_tables(&raw_sections).expect("flame decode");
    assert_eq!(flame_only.capture.samples.len(), capture.samples.len());
    for (got, want) in flame_only.capture.samples.iter().zip(&capture.samples) {
        assert_eq!(got.time, want.time);
        assert_eq!(got.weight, want.weight);
        assert_eq!(got.stack, want.stack);
        assert_eq!(got.task.get(), NONE);
    }
    assert!(flame_only.capture.syscalls.is_empty());
    assert!(flame_only.capture.syscall_blobs.is_empty());
}

#[test]
fn time_rounding_and_org_mismatch_and_checksum_checks() {
    let mut capture = build_rich_capture();
    round_capture_times(&mut capture, TimeUnit::Ms);
    for sample in &capture.samples {
        assert_eq!(sample.time.get() % 1_000_000, 0);
    }

    let data = ChunkData {
        capture,
        chunks: vec![ChunkRow {
            agent: 1,
            number: 1,
            start: 1_000_000_000,
            end: 1_005_000_000,
            sampled_ns: 5_000_000_000,
            preset: 0,
            time_unit: TimeUnit::Ms,
            policy: 0,
            rules: String::new(),
        }],
        dimensions: Vec::new(),
        sanitizer_counts: Vec::new(),
    };

    let sections = encode_tables(&data, TimeUnit::Ms, DEFAULT_ZSTD_LEVEL, 0).expect("encode");
    let mut writer = ObjectWriter::new(ObjectKind::WriteAhead, ObjectLevel::None, 1, 1);
    writer
        .add_part(&PartSpec {
            org_id: 10,
            project_id: 0,
            starts_at: 1_000_000_000,
            ends_at: 1_005_000_000,
            signal: Signal::Profile,
            part: PartKind::Chunks,
            sections,
        })
        .expect("add part");
    let (mut bytes, _) = writer.finish().expect("finish");

    let reader = ObjectReader::open(&bytes).expect("open");
    assert!(matches!(
        reader.part_sections(0, Some(99)),
        Err(ObjectError::OrgMismatch {
            expected: 99,
            found: 10
        })
    ));

    let raw = reader.part_sections(0, Some(10)).expect("valid org");
    let decoded = decode_tables(&raw).expect("decode ms");
    for sample in &decoded.capture.samples {
        assert_eq!(sample.time.get() % 1_000_000, 0);
    }

    bytes[40] ^= 0xff;
    let reader = ObjectReader::open(&bytes).expect("part list still intact");
    assert!(matches!(
        reader.part_sections(0, Some(10)),
        Err(ObjectError::Checksum { what: "section" })
    ));
}

#[test]
fn sanitize_chunk_scrubs_every_planted_secret_before_encoding() {
    let capture = build_rich_capture();
    let raw = ChunkData {
        capture,
        chunks: vec![ChunkRow {
            agent: 5,
            number: 1,
            start: 1_000_000_000,
            end: 1_005_000_000,
            sampled_ns: 5_000_000_000,
            preset: 1,
            time_unit: TimeUnit::Ns,
            policy: 0,
            rules: String::new(),
        }],
        dimensions: vec![
            DimensionRow {
                process: 0,
                name: "service.name".into(),
                value: "payments".into(),
            },
            DimensionRow {
                process: 0,
                name: "process.command_args".into(),
                value: "--api-key=secret_key_value_999".into(),
            },
        ],
        sanitizer_counts: Vec::new(),
    };

    let policy = Sanitizer::new(&Builtin::DEFAULT, Vec::new(), HashKey::new(b"test-key"));
    let sanitized = sanitize_chunk(raw, TimeUnit::Ns, &policy);
    assert!(!sanitized.data().sanitizer_counts.is_empty());

    let post = encode_post(&[sanitized], 5, 1_005_000_000, DEFAULT_ZSTD_LEVEL).expect("post");
    let reader = ObjectReader::open(post.as_bytes()).expect("open post");
    let decoded = decode_tables(&reader.part_sections(0, None).expect("sections")).expect("decode");

    let all_strings = String::from_utf8_lossy(&decoded.capture.strings);
    let all_blobs = String::from_utf8_lossy(&decoded.capture.syscall_blobs);
    for secret in [
        "supersecret123",
        "hunter2",
        "tok_secret_abc1234567890",
        "AKIAIOSFODNN7EXAMPLE",
        "my_db_password",
        "secret_key_value_999",
    ] {
        assert!(!all_strings.contains(secret), "leaked {secret} in strings");
        assert!(!all_blobs.contains(secret), "leaked {secret} in blobs");
        assert!(
            !post
                .as_bytes()
                .windows(secret.len())
                .any(|w| w == secret.as_bytes()),
            "leaked {secret} in raw post bytes"
        );
    }

    assert!(
        decoded
            .dimensions
            .iter()
            .any(|d| d.name == "service.name" && d.value == "payments")
    );
    assert!(all_strings.contains("hot_loop"));
}

#[test]
fn merge_chunks_shares_strings_and_stacks_and_preserves_chunk_numbers() {
    let c1 = ChunkData {
        capture: build_rich_capture(),
        chunks: vec![ChunkRow {
            agent: 1,
            number: 10,
            start: 1_000_000_000,
            end: 1_005_000_000,
            sampled_ns: 5_000_000_000,
            preset: 0,
            time_unit: TimeUnit::Ms,
            policy: 0,
            rules: String::new(),
        }],
        dimensions: vec![DimensionRow {
            process: 0,
            name: "service.name".into(),
            value: "svc".into(),
        }],
        sanitizer_counts: Vec::new(),
    };
    let c2 = ChunkData {
        capture: build_rich_capture(),
        chunks: vec![ChunkRow {
            agent: 1,
            number: 11,
            start: 1_005_000_000,
            end: 1_010_000_000,
            sampled_ns: 5_000_000_000,
            preset: 0,
            time_unit: TimeUnit::Ms,
            policy: 0,
            rules: String::new(),
        }],
        dimensions: vec![DimensionRow {
            process: 0,
            name: "service.name".into(),
            value: "svc".into(),
        }],
        sanitizer_counts: Vec::new(),
    };

    let merged = merge_chunks(&[c1, c2]).expect("merge");
    assert_eq!(merged.chunks.len(), 2);
    assert_eq!(merged.chunks[0].number, 10);
    assert_eq!(merged.chunks[1].number, 11);
    assert_eq!(merged.capture.samples.len(), 6);
    assert_eq!(merged.capture.stacks.len(), 2);
    assert_eq!(merged.dimensions.len(), 1);
}

proptest! {
    #[test]
    fn zigzag_round_trips(value in any::<i64>()) {
        prop_assert_eq!(zigzag_decode(zigzag_encode(value)), value);
    }

    #[test]
    fn varints_round_trip(values in prop::collection::vec(any::<u64>(), 0..200)) {
        let encoded = encode_varints(&values);
        let decoded = decode_varints(&encoded, values.len()).expect("decode");
        prop_assert_eq!(decoded, values);
    }

    #[test]
    fn bitpacked_round_trip(values in prop::collection::vec(any::<u64>(), 0..200)) {
        let encoded = encode_bitpacked(&values);
        let decoded = decode_bitpacked(&encoded, values.len()).expect("decode");
        prop_assert_eq!(decoded, values);
    }

    #[test]
    fn delta_and_relative_round_trip(
        uvals in prop::collection::vec(any::<u64>(), 0..100),
        ivals in prop::collection::vec(any::<i64>(), 0..100),
        base in any::<i64>(),
    ) {
        prop_assert_eq!(&delta_decode_u64(&delta_encode_u64(&uvals)), &uvals);
        prop_assert_eq!(&delta_decode_i64(&delta_encode_i64(&ivals, base), base), &ivals);
        let keys: Vec<u32> = (0..uvals.len())
            .map(|i| u32::try_from(i % 5).expect("mod 5"))
            .collect();
        let dbk = delta_by_key_encode_u64(&uvals, &keys);
        prop_assert_eq!(&delta_by_key_decode_u64(&dbk, &keys).expect("dbk"), &uvals);
        let anchor: Vec<i64> = ivals.iter().map(|v| v.wrapping_sub(17)).collect();
        let rel = relative_encode_i64(&ivals, &anchor);
        prop_assert_eq!(&relative_decode_i64(&rel, &anchor).expect("rel"), &ivals);
    }

    #[test]
    fn dict_and_words_round_trip(
        values in prop::collection::vec(0_u64..16, 0..150),
        count in 0_usize..20,
        words in 1_usize..4,
    ) {
        let encoded = encode_dict_u64(&values);
        prop_assert_eq!(decode_dict_u64(&encoded, values.len()).expect("dict"), values);

        let stride = words * 8;
        let records: Vec<Vec<u8>> = (0..count)
            .map(|r| {
                (0..stride)
                    .map(|b| u8::try_from((r * 31 + b) & 0xff).expect("byte"))
                    .collect()
            })
            .collect();
        let refs: Vec<&[u8]> = records.iter().map(Vec::as_slice).collect();
        let encoded = encode_words(&refs, stride);
        let decoded = decode_words(&encoded, count, stride).expect("words");
        let flat: Vec<u8> = records.into_iter().flatten().collect();
        prop_assert_eq!(decoded, flat);
    }

    #[test]
    fn arbitrary_bytes_never_panic_object_reader(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        if let Ok(reader) = ObjectReader::open(&bytes) {
            for idx in 0..reader.parts().len() {
                if let Ok(sections) = reader.part_sections(idx, None) {
                    let _ = decode_tables(&sections);
                    let _ = decode_flame_tables(&sections);
                }
            }
        }
    }
}

#[test]
fn section_kind_flame_filter_covers_core_tables() {
    assert!(SectionKind::Samples.needed_for_flame());
    assert!(SectionKind::Functions.needed_for_flame());
    assert!(!SectionKind::Syscalls.needed_for_flame());
    assert!(!SectionKind::SyscallBlobs.needed_for_flame());
}

#[test]
fn object_reader_rejects_overlapping_parts_and_multipart_posts() {
    let make_strings = || {
        let mut sw = SectionWriter::new(
            SectionKind::Strings,
            1,
            TimeUnit::Ns,
            1,
            0,
            DEFAULT_ZSTD_LEVEL,
        );
        sw.add_column(WHOLE_SECTION_GROUP, 0, Coding::Bytes, 0, 0, b"ok\0")
            .expect("add_column");
        sw.finish(0).expect("finish section")
    };
    let mut writer = ObjectWriter::new(ObjectKind::WriteAhead, ObjectLevel::None, 100, 1);
    for _ in 0..2 {
        writer
            .add_part(&PartSpec {
                org_id: 1,
                project_id: 1,
                starts_at: 100,
                ends_at: 200,
                signal: Signal::Profile,
                part: PartKind::Chunks,
                sections: vec![make_strings()],
            })
            .expect("add part");
    }
    let (bytes, _) = writer.finish().expect("finish wal");
    assert!(ObjectReader::open(&bytes).is_ok());

    let mut overlapping = bytes.clone();
    let len = overlapping.len();
    let trailer_start = len - 24;
    let list_len = u64::from_le_bytes(
        overlapping[trailer_start..trailer_start + 8]
            .try_into()
            .expect("u64"),
    );
    let list_start = trailer_start - usize::try_from(list_len).expect("usize");
    let part1_first_section = list_start + 16 + 64 + 48;
    overlapping[part1_first_section..part1_first_section + 4].copy_from_slice(&0_u32.to_le_bytes());
    let new_sum = twox_hash::XxHash3_64::oneshot(&overlapping[list_start..trailer_start]);
    overlapping[trailer_start + 8..trailer_start + 16].copy_from_slice(&new_sum.to_le_bytes());
    assert!(matches!(
        ObjectReader::open(&overlapping),
        Err(ObjectError::Layout(_))
    ));

    let mut multipart_post = bytes;
    multipart_post[12] = ObjectKind::Post as u8;
    assert!(matches!(
        ObjectReader::open(&multipart_post),
        Err(ObjectError::Limit(_))
    ));
}

#[test]
fn decode_tables_enforces_cumulative_decompression_budget() {
    let mut strings = vec![b'a'; 48 * 1024 * 1024];
    strings.push(0);
    let rows = 1_200_000_u32;
    let varint_zeros = vec![0_u8; rows as usize];
    let mut sw = SectionWriter::new(
        SectionKind::Strings,
        1,
        TimeUnit::Ns,
        1,
        0,
        DEFAULT_ZSTD_LEVEL,
    );
    sw.add_column(WHOLE_SECTION_GROUP, 0, Coding::Bytes, 0, 0, &strings)
        .expect("strings col");
    let mut sections = vec![sw.finish(0).expect("strings section")];
    for (kind, u32_cols, u8_cols) in [
        (SectionKind::Lines, 4_u8, 0_u8),
        (SectionKind::Functions, 4, 1),
        (SectionKind::Sites, 4, 1),
    ] {
        let mut sw = SectionWriter::new(kind, 1, TimeUnit::Ns, rows, 0, DEFAULT_ZSTD_LEVEL);
        for id in 0..u32_cols {
            sw.add_column(WHOLE_SECTION_GROUP, id, Coding::Plain, 0, 0, &varint_zeros)
                .expect("u32 col");
        }
        for offset in 0..u8_cols {
            sw.add_column(
                WHOLE_SECTION_GROUP,
                u32_cols + offset,
                Coding::Bytes,
                0,
                0,
                &varint_zeros,
            )
            .expect("u8 col");
        }
        sections.push(sw.finish(0).expect("section"));
    }
    let mut writer = ObjectWriter::new(ObjectKind::Post, ObjectLevel::None, 100, 1);
    writer
        .add_part(&PartSpec {
            org_id: 1,
            project_id: 1,
            starts_at: 100,
            ends_at: 200,
            signal: Signal::Profile,
            part: PartKind::Chunks,
            sections,
        })
        .expect("add part");
    let (bytes, _) = writer.finish().expect("finish post");
    let reader = ObjectReader::open(&bytes).expect("open");
    let raw_sections = reader.part_sections(0, None).expect("sections");
    assert!(matches!(
        decode_tables(&raw_sections),
        Err(ObjectError::Limit(_))
    ));
}

#[test]
fn shift_capture_times_shifts_all_timestamps_and_preserves_unseen() {
    let mut capture = build_rich_capture();
    let delta = 1_700_000_000_000_000_000_i64;
    let (before_start, before_end) = capture.span();
    let before_sample = capture.samples[0].time.get();
    let before_proc = capture.processes[0].start.get();
    capture.syscalls[0].entered = crate::capture::records::UNSEEN.into();
    let before_exit = capture.syscalls[0].exited.get();

    shift_capture_times(&mut capture, delta);

    assert_eq!(capture.span(), (before_start + delta, before_end + delta));
    assert_eq!(capture.samples[0].time.get(), before_sample + delta);
    assert_eq!(capture.processes[0].start.get(), before_proc + delta);
    assert_eq!(
        capture.syscalls[0].entered.get(),
        crate::capture::records::UNSEEN
    );
    assert_eq!(capture.syscalls[0].exited.get(), before_exit + delta);
}

#[test]
fn decode_tables_enforces_budget_on_repeated_string_references() {
    let mut strings = vec![b'x'; 2 * 1024 * 1024];
    strings.push(0);
    let mut sw = SectionWriter::new(
        SectionKind::Strings,
        1,
        TimeUnit::Ns,
        1,
        0,
        DEFAULT_ZSTD_LEVEL,
    );
    sw.add_column(WHOLE_SECTION_GROUP, 0, Coding::Bytes, 0, 0, &strings)
        .expect("strings col");
    let strings_sec = sw.finish(0).expect("strings section");

    let rows = 80_u32;
    let zeros = vec![0_u8; rows as usize];
    let mut dw = SectionWriter::new(
        SectionKind::Dimensions,
        1,
        TimeUnit::Ns,
        rows,
        0,
        DEFAULT_ZSTD_LEVEL,
    );
    for col in 0_u8..3 {
        dw.add_column(WHOLE_SECTION_GROUP, col, Coding::Plain, 0, 0, &zeros)
            .expect("dim col");
    }
    let dims_sec = dw.finish(0).expect("dims section");

    let mut writer = ObjectWriter::new(ObjectKind::Post, ObjectLevel::None, 100, 1);
    writer
        .add_part(&PartSpec {
            org_id: 1,
            project_id: 1,
            starts_at: 100,
            ends_at: 200,
            signal: Signal::Profile,
            part: PartKind::Chunks,
            sections: vec![strings_sec, dims_sec],
        })
        .expect("add part");
    let (bytes, _) = writer.finish().expect("finish");
    let reader = ObjectReader::open(&bytes).expect("open");
    let raw_sections = reader.part_sections(0, None).expect("sections");
    assert!(matches!(
        decode_tables(&raw_sections),
        Err(ObjectError::Limit(_))
    ));
}
