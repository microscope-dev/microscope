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
    Coding, DecodeBudget, SectionReader, SectionWriter, WHOLE_SECTION_GROUP, decode_binned_2b,
    decode_bp_tree, decode_dict_u64, decode_nonzero_u64, decode_sparse_histograms, decode_varints,
    decode_words, delta_by_key_decode_u64, delta_by_key_encode_u64, delta_decode_i64,
    delta_decode_u64, delta_encode_i64, delta_encode_u64, encode_binned_2b, encode_bp_tree,
    encode_dict_u64, encode_nonzero_u64, encode_sparse_histograms, encode_varints, encode_words,
    forest_to_dfs, relative_decode_i64, relative_encode_i64, zigzag_decode, zigzag_encode,
};
use super::filter::{
    BinaryFuse8, filter_key_dimension, filter_key_frame_addr, filter_key_function_name,
    filter_key_process_name,
};
use super::{
    ChunkData, ChunkRow, DEFAULT_PAGE_ROWS, DEFAULT_ZSTD_LEVEL, DimensionRow, OBJECT_HEADER_LEN,
    ObjectError, ObjectKind, ObjectLevel, ObjectReader, ObjectWriter, PartKind, PartSpec,
    SECTION_CHECKSUM_LEN, SanitizerCountRow, SectionKind, SectionMeta, Signal, TRAILER_LEN,
    TimeUnit, decode_flame_tables, decode_section_index, decode_tables, encode_section_index,
    encode_tables, merge_chunks, part_list_checksum, round_capture_times, shift_capture_times,
    verify_section_slice,
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
    assert_eq!(metas[0].project_id, 7);

    if let Some(path) = std::env::var_os("WRITE_CHUNK_FUZZ_SEED") {
        std::fs::write(path, &bytes).expect("write chunk fuzz seed");
    }

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
fn time_rounding_and_org_mismatch_and_keyed_checksum_checks() {
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
    let key_seed = writer.key_seed();
    writer
        .add_part(&PartSpec {
            org_id: 10,
            project_id: 77,
            starts_at: 1_000_000_000,
            ends_at: 1_005_000_000,
            signal: Signal::Profile,
            part: PartKind::Chunks,
            sections,
        })
        .expect("add part");
    let (mut bytes, metas) = writer.finish().expect("finish");

    let first_sec = &metas[0].sections[0];
    let start = usize::try_from(first_sec.offset).expect("off");
    let end = start + first_sec.stored_len as usize;
    let slice = &bytes[start..end];
    let verified = verify_section_slice(slice, first_sec.offset, first_sec, key_seed, 10, 77, 0)
        .expect("valid 1-RTT slice");
    assert_eq!(
        verified.bytes.len() + SECTION_CHECKSUM_LEN,
        first_sec.stored_len as usize
    );

    assert!(matches!(
        verify_section_slice(slice, first_sec.offset, first_sec, key_seed, 11, 77, 0),
        Err(ObjectError::Checksum { what: "section" })
    ));
    assert!(matches!(
        verify_section_slice(slice, first_sec.offset, first_sec, key_seed, 10, 78, 0),
        Err(ObjectError::Checksum { what: "section" })
    ));
    assert!(matches!(
        verify_section_slice(slice, first_sec.offset, first_sec, key_seed, 10, 77, 1),
        Err(ObjectError::Checksum { what: "section" })
    ));
    assert!(matches!(
        verify_section_slice(slice, first_sec.offset, first_sec, key_seed ^ 1, 10, 77, 0),
        Err(ObjectError::Checksum { what: "section" })
    ));

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

    bytes[OBJECT_HEADER_LEN] ^= 0xff;
    let reader = ObjectReader::open(&bytes).expect("part list still intact");
    assert!(matches!(
        reader.part_sections(0, Some(10)),
        Err(ObjectError::Checksum { what: "section" })
    ));
}

#[expect(
    clippy::too_many_lines,
    reason = "verifies agent sanitization, verbatim server passthrough, and dirty fallback"
)]
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

    let post_sections = reader.part_sections(0, None).expect("post sections");
    let (resan, verbatim) =
        crate::sanitize::re_sanitize_sections_verbatim(&post_sections, &policy, DEFAULT_ZSTD_LEVEL)
            .expect("re_sanitize clean");
    assert!(verbatim, "already-sanitized post should pass verbatim");
    assert_eq!(resan.len(), post_sections.len());
    for (got, want) in resan.iter().zip(&post_sections) {
        assert_eq!(got.kind, want.kind);
        assert_eq!(got.bytes, want.bytes);
    }

    let (meta_chunks, meta_dims) =
        crate::sanitize::decode_post_metadata(&post_sections).expect("meta decode");
    assert_eq!(meta_chunks.len(), 1);
    assert!(
        meta_dims
            .iter()
            .any(|d| d.name == "service.name" && d.value == "payments")
    );

    let dirty = ChunkData {
        capture: build_rich_capture(),
        chunks: decoded.chunks,
        dimensions: vec![DimensionRow {
            process: 0,
            name: "process.command_args".into(),
            value: "--token=unsanitized_secret_999".into(),
        }],
        sanitizer_counts: Vec::new(),
    };
    let dirty_sections = encode_tables(&dirty, TimeUnit::Ns, DEFAULT_ZSTD_LEVEL, 0).expect("enc");
    let mut dirty_writer = ObjectWriter::new(ObjectKind::Post, ObjectLevel::None, 1, 5);
    dirty_writer
        .add_part(&PartSpec {
            org_id: 0,
            project_id: 0,
            starts_at: 1_000_000_000,
            ends_at: 1_005_000_000,
            signal: Signal::Profile,
            part: PartKind::Chunks,
            sections: dirty_sections,
        })
        .expect("add");
    let (dirty_bytes, _) = dirty_writer.finish().expect("fin");
    let dirty_reader = ObjectReader::open(&dirty_bytes).expect("open dirty");
    let dirty_raw = dirty_reader.part_sections(0, None).expect("dirty raw");
    let (scrubbed_secs, verbatim_dirty) =
        crate::sanitize::re_sanitize_sections_verbatim(&dirty_raw, &policy, DEFAULT_ZSTD_LEVEL)
            .expect("re_sanitize dirty");
    assert!(!verbatim_dirty, "dirty post must trigger fallback scrub");
    let mut check_w = ObjectWriter::new(ObjectKind::WriteAhead, ObjectLevel::None, 1, 5);
    check_w
        .add_part(&PartSpec {
            org_id: 1,
            project_id: 2,
            starts_at: 1_000_000_000,
            ends_at: 1_005_000_000,
            signal: Signal::Profile,
            part: PartKind::Chunks,
            sections: scrubbed_secs,
        })
        .expect("add wal");
    let (wal_bytes, _) = check_w.finish().expect("fin wal");
    let wal_reader = ObjectReader::open(&wal_bytes).expect("open wal");
    let scrubbed = decode_tables(&wal_reader.part_sections(0, Some(1)).expect("wal secs"))
        .expect("decode scrubbed");
    let scrubbed_strings = String::from_utf8_lossy(&scrubbed.capture.strings);
    assert!(!scrubbed_strings.contains("unsanitized_secret_999"));
    assert!(!scrubbed_strings.contains("supersecret123"));
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

#[test]
fn binary_fuse8_zero_false_negatives_and_low_fpr() {
    for &count in &[0_usize, 1, 2, 3, 17, 250, 4_096, 20_000] {
        let keys: Vec<u64> = (0..count)
            .map(|i| {
                if i % 4 == 0 {
                    filter_key_frame_addr("build-abc", i as u64 * 16)
                } else if i % 4 == 1 {
                    filter_key_function_name(&format!("fn_{i}"))
                } else if i % 4 == 2 {
                    filter_key_dimension("service.name", &format!("svc-{i}"))
                } else {
                    filter_key_process_name(&format!("proc-{i}"))
                }
            })
            .collect();

        let filter = BinaryFuse8::build(&keys);
        assert_eq!(filter.is_empty(), count == 0);
        for &k in &keys {
            assert!(filter.contains(k), "false negative for count={count}");
        }

        let wire = filter.to_bytes();
        let decoded = BinaryFuse8::from_bytes(&wire).expect("decode filter");
        assert_eq!(decoded, filter);
        for &k in &keys {
            assert!(decoded.contains(k), "false negative after decode");
        }

        if count >= 250 {
            let probes = 10_000_usize;
            let false_positives = (0..probes)
                .filter(|&p| decoded.contains(filter_key_function_name(&format!("absent_{p}"))))
                .count();
            assert!(
                false_positives < probes * 12 / 1000,
                "too many false positives for count={count}: {false_positives}/{probes}"
            );
        }
    }

    assert!(BinaryFuse8::from_bytes(&[]).is_err());
    assert!(BinaryFuse8::from_bytes(&[99; 20]).is_err());
}

#[test]
fn paged_off_cpu_and_syscalls_round_trip_beyond_single_page() {
    let mut b = CaptureBuilder::new();
    let module = b.module("/bin/app", Some("abcd"), [1; 16], 4096);
    let proc_id = b.process(Pid::new(1), 1_000);
    let t1 = b.thread(Pid::new(1), Tid::new(1), 1_000);
    let f1 = b.frame(FrameKind::Native, module, 0x100, 0);
    let stack = b.stack(&[f1]);

    let n_off = DEFAULT_PAGE_ROWS + 5_000;
    for i in 0..n_off {
        let time = 10_000 + i64::try_from(i).expect("i") * 1_003;
        b.off_cpu(t1, time, 500 + (i as u64 % 97), i % 2 == 0, 0);
    }

    for i in 0..1_200_usize {
        let entered = 10_000 + i64::try_from(i).expect("i") * 2_000;
        b.syscall(
            Syscall {
                entered: entered.into(),
                exited: (entered + 350).into(),
                ret: 0.into(),
                args: [
                    (i as u64).into(),
                    1.into(),
                    2.into(),
                    3.into(),
                    4.into(),
                    5.into(),
                ],
                thread: t1.into(),
                number: 9.into(), // mmap has 6 args on x86_64
                stack: stack.into(),
                captures: NONE.into(),
                captures_len: 0.into(),
                abi: 0,
                outcome: CallOutcome::Returned as u8,
                flags: 0,
                reserved: 0,
            },
            &[],
        );
    }

    let capture = b.finish();
    let data = ChunkData {
        capture: capture.clone(),
        chunks: Vec::new(),
        dimensions: Vec::new(),
        sanitizer_counts: Vec::new(),
    };

    let sections = encode_tables(&data, TimeUnit::Ns, DEFAULT_ZSTD_LEVEL, 0).expect("encode");
    let off_sec = sections
        .iter()
        .find(|s| s.kind == SectionKind::OffCpu)
        .expect("off_cpu section");
    let budget = DecodeBudget::new(super::MAX_DECOMPRESSED_OBJECT_BYTES);
    let off_reader = SectionReader::open_budgeted(&off_sec.bytes, SectionKind::OffCpu, &budget)
        .expect("open off_cpu");
    assert_eq!(off_reader.page_count(), 2);
    assert_eq!(off_reader.pages_overlapping(0, 15_000).len(), 1);

    let sys_sec = sections
        .iter()
        .find(|s| s.kind == SectionKind::Syscalls)
        .expect("syscalls section");
    let sys_reader = SectionReader::open_budgeted(&sys_sec.bytes, SectionKind::Syscalls, &budget)
        .expect("open syscalls");
    let core_only = sys_reader.read_page_frames(0, 1).expect("read core frame");
    assert!(core_only.column(0, 0).is_ok());
    assert!(core_only.column(1, 0).is_err());

    let mut writer = ObjectWriter::new(ObjectKind::Capture, ObjectLevel::None, 1, 1);
    writer
        .add_part(&PartSpec {
            org_id: 0,
            project_id: 0,
            starts_at: 10_000,
            ends_at: 200_000_000,
            signal: Signal::Profile,
            part: PartKind::Chunks,
            sections,
        })
        .expect("add part");
    let (bytes, _) = writer.finish().expect("finish");

    let reader = ObjectReader::open(&bytes).expect("open");
    let decoded = decode_tables(&reader.part_sections(0, None).expect("parts")).expect("decode");
    assert_eq!(decoded.capture.off_cpu.len(), capture.off_cpu.len());
    assert_eq!(decoded.capture.off_cpu, capture.off_cpu);
    assert_eq!(decoded.capture.syscalls, capture.syscalls);
    let _ = proc_id;
}

#[test]
fn recoverable_reader_handles_checkpoints_and_rejects_corrupt_trailer() {
    let make_sec = |s: &[u8]| {
        let mut sw = SectionWriter::new(
            SectionKind::Strings,
            1,
            TimeUnit::Ns,
            1,
            0,
            DEFAULT_ZSTD_LEVEL,
        );
        sw.add_column(WHOLE_SECTION_GROUP, 0, Coding::Bytes, 0, s)
            .expect("col");
        sw.finish(0).expect("finish")
    };

    let mut writer = ObjectWriter::new(ObjectKind::Capture, ObjectLevel::None, 100, 1);
    writer
        .add_part(&PartSpec {
            org_id: 0,
            project_id: 0,
            starts_at: 100,
            ends_at: 200,
            signal: Signal::Profile,
            part: PartKind::Chunks,
            sections: vec![make_sec(b"first\0")],
        })
        .expect("p1");
    writer.write_checkpoint().expect("write cp1");
    let mut full = writer.take_bytes().expect("take cp1");
    let len_after_cp1 = full.len();

    writer
        .add_part(&PartSpec {
            org_id: 0,
            project_id: 0,
            starts_at: 200,
            ends_at: 300,
            signal: Signal::Profile,
            part: PartKind::Chunks,
            sections: vec![make_sec(b"second\0")],
        })
        .expect("p2");
    writer.write_checkpoint().expect("write cp2");
    writer.write_final_footer().expect("write final");
    full.extend_from_slice(&writer.take_bytes().expect("take cp2"));

    let r_full = ObjectReader::open_recoverable(&full).expect("open full");
    assert!(!r_full.truncated);
    assert_eq!(r_full.parts().len(), 2);

    let cut = len_after_cp1 + 5;
    let r_trunc = ObjectReader::open_recoverable(&full[..cut]).expect("recover cp1");
    assert!(r_trunc.truncated);
    assert_eq!(r_trunc.parts().len(), 1);

    let mut bad = full;
    let checksum_byte = bad.len() - 10;
    bad[checksum_byte] ^= 0x01;
    assert!(ObjectReader::open_recoverable(&bad).is_err());
}

#[test]
fn binned_2b_extremes_and_malformed_inputs() {
    let values = vec![
        0,
        1,
        2,
        3,
        4,
        7,
        8,
        15,
        16,
        1_000,
        1_000_000_000,
        (1_u64 << 62) - 1,
        1_u64 << 62,
        (1_u64 << 63) - 1,
        1_u64 << 63,
        u64::MAX - 1,
        u64::MAX,
    ];
    let (classes, mantissa) = encode_binned_2b(&values);
    let decoded = decode_binned_2b(&classes, &mantissa, values.len()).expect("decode extremes");
    assert_eq!(decoded, values);

    assert!(decode_binned_2b(&[252], &[], 1).is_err());
    assert!(decode_binned_2b(&[255], &[0; 16], 1).is_err());
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
    fn binned_2b_round_trip(values in prop::collection::vec(any::<u64>(), 0..300)) {
        let (classes, mantissa) = encode_binned_2b(&values);
        let decoded = decode_binned_2b(&classes, &mantissa, values.len()).expect("decode");
        prop_assert_eq!(decoded, values);
    }

    #[test]
    fn bp_tree_forest_round_trip(
        parents in prop::collection::vec(0_u32..32, 0..120),
    ) {
        let valid_parents: Vec<u32> = parents
            .iter()
            .enumerate()
            .map(|(i, &p)| {
                if i == 0 || p % 5 == 0 {
                    NONE
                } else {
                    p % u32::try_from(i).expect("i")
                }
            })
            .collect();
        let (dfs_parents, old_to_dfs, dfs_to_old) = forest_to_dfs(&valid_parents);
        prop_assert_eq!(dfs_parents.len(), valid_parents.len());
        prop_assert_eq!(dfs_to_old.len(), valid_parents.len());

        let bp = encode_bp_tree(&dfs_parents);
        let decoded = decode_bp_tree(&bp, valid_parents.len()).expect("decode bp");
        prop_assert_eq!(&decoded, &dfs_parents);

        for (old, &old_p) in valid_parents.iter().enumerate() {
            let new_i = old_to_dfs[old] as usize;
            let expected_p = if old_p == NONE {
                NONE
            } else {
                old_to_dfs[old_p as usize]
            };
            prop_assert_eq!(decoded[new_i], expected_p);
        }
    }

    #[test]
    fn sparse_histogram40_round_trip(
        rows in prop::collection::vec(
            prop::collection::vec((0_usize..40, 0_u64..10_000), 0..8),
            0..40,
        )
    ) {
        let hists: Vec<[u64; 40]> = rows
            .iter()
            .map(|entries| {
                let mut h = [0_u64; 40];
                for &(bucket, val) in entries {
                    h[bucket] = val;
                }
                h
            })
            .collect();
        let encoded = encode_sparse_histograms(&hists);
        let decoded = decode_sparse_histograms(&encoded, hists.len(), None).expect("decode hist");
        prop_assert_eq!(decoded, hists);
    }

    #[test]
    fn non_zero_bitmap_round_trip(
        values in prop::collection::vec(
            prop_oneof![Just(0_u64), any::<u64>()],
            0..250,
        )
    ) {
        let encoded = encode_nonzero_u64(&values);
        let decoded = decode_nonzero_u64(&encoded, values.len()).expect("decode nz");
        prop_assert_eq!(decoded, values);
    }

    #[test]
    fn binary_fuse8_proptest_no_false_negatives(
        keys in prop::collection::vec(any::<u64>(), 0..500),
    ) {
        let filter = BinaryFuse8::build(&keys);
        let wire = filter.to_bytes();
        let decoded = BinaryFuse8::from_bytes(&wire).expect("decode");
        for &k in &keys {
            prop_assert!(decoded.contains(k));
        }
    }

    #[test]
    fn section_index_round_trip(
        part_index in any::<u32>(),
        entries in prop::collection::vec(
            (any::<u16>(), any::<u64>(), 0_u64..1_000_000_000, any::<u32>(), any::<u32>(), any::<u64>()),
            0..32,
        )
    ) {
        let metas: Vec<SectionMeta> = entries
            .into_iter()
            .map(|(kind, key, offset, stored_len, rows, checksum)| SectionMeta {
                kind,
                key,
                offset,
                stored_len,
                rows,
                checksum,
            })
            .collect();
        let encoded = encode_section_index(part_index, &metas);
        let (got_part, got_metas) = decode_section_index(&encoded).expect("decode index");
        prop_assert_eq!(got_part, part_index);
        prop_assert_eq!(got_metas, metas);
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
        let _ = BinaryFuse8::from_bytes(&bytes);
        let _ = decode_section_index(&bytes);
        if let Ok(reader) = ObjectReader::open(&bytes) {
            for idx in 0..reader.parts().len() {
                if let Ok(sections) = reader.part_sections(idx, None) {
                    let _ = decode_tables(&sections);
                    let _ = decode_flame_tables(&sections);
                }
            }
        }
        let _ = ObjectReader::open_recoverable(&bytes);
    }
}

#[test]
fn event_bundle_round_trips_with_shared_tier_a_symbols() {
    let capture = build_rich_capture();
    let data = ChunkData {
        capture: capture.clone(),
        chunks: vec![ChunkRow {
            agent: 9,
            number: 1,
            start: 1_000_000_000,
            end: 1_005_000_000,
            sampled_ns: 4_800_000_000,
            preset: 0,
            time_unit: TimeUnit::Ns,
            policy: 1,
            rules: "demo".into(),
        }],
        dimensions: vec![DimensionRow {
            process: 0,
            name: "service.name".into(),
            value: "demo-svc".into(),
        }],
        sanitizer_counts: Vec::new(),
    };
    let mut shared_strings = capture.strings.clone();
    for extra in ["demo", "service.name", "demo-svc"] {
        shared_strings.extend_from_slice(extra.as_bytes());
        shared_strings.push(0);
    }

    let shared_proc_ids: Vec<u32> = (0..capture.processes.len())
        .map(|i| u32::try_from(i).expect("u32"))
        .collect();
    let bundle_secs = super::encode_event_bundle(
        &data,
        &shared_strings,
        &shared_proc_ids,
        TimeUnit::Ns,
        DEFAULT_ZSTD_LEVEL,
        9,
    )
    .expect("encode bundle");
    for sec in &bundle_secs {
        assert!(!matches!(
            sec.kind,
            SectionKind::Strings
                | SectionKind::Modules
                | SectionKind::Frames
                | SectionKind::FrameSymbols
                | SectionKind::Lines
                | SectionKind::Functions
                | SectionKind::Stacks
                | SectionKind::Sites
        ));
    }

    let mut w = ObjectWriter::new(ObjectKind::Detail, ObjectLevel::Minute, 1, 1);
    w.add_part(&PartSpec {
        org_id: 1,
        project_id: 1,
        starts_at: 1_000_000_000,
        ends_at: 1_005_000_000,
        signal: Signal::Profile,
        part: PartKind::Detail,
        sections: bundle_secs,
    })
    .expect("add part");
    let (bytes, _) = w.finish().expect("finish");
    let reader = ObjectReader::open(&bytes).expect("open");
    let raw = reader.part_sections(0, Some(1)).expect("sections");

    let shared = super::SharedSymbols {
        strings: &shared_strings,
        modules: &capture.modules,
        processes: &capture.processes,
        frames: &capture.frames,
        frame_symbols: &capture.frame_symbols,
        lines: &capture.lines,
        functions: &capture.functions,
        stacks: &capture.stacks,
        sites: &capture.sites,
    };
    let decoded = super::decode_event_bundle(&raw, &shared, false).expect("decode full bundle");
    assert_eq!(decoded.chunks, data.chunks);
    assert_eq!(decoded.dimensions, data.dimensions);
    assert_eq!(decoded.capture.samples, capture.samples);
    assert_eq!(decoded.capture.off_cpu, capture.off_cpu);
    assert_eq!(decoded.capture.syscalls, capture.syscalls);
    assert_eq!(decoded.capture.syscall_stats, capture.syscall_stats);

    let flame = super::decode_event_bundle(&raw, &shared, true).expect("decode flame bundle");
    assert_eq!(flame.capture.samples.len(), capture.samples.len());
    assert!(flame.capture.syscalls.is_empty());

    let tight_budget = DecodeBudget::new(64 * 1024);
    let light_flame =
        super::decode_flame_bundle(&raw, &shared, &tight_budget).expect("decode light flame");
    assert_eq!(light_flame.samples.len(), capture.samples.len());
    for (got, expected) in light_flame.samples.iter().zip(&capture.samples) {
        assert_eq!(got.time, expected.time);
        assert_eq!(got.weight, expected.weight);
        assert_eq!(got.stack, expected.stack);
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
        sw.add_column(WHOLE_SECTION_GROUP, 0, Coding::Bytes, 0, b"ok\0")
            .expect("add_column");
        sw.finish(0).expect("finish section")
    };
    let mut writer = ObjectWriter::new(ObjectKind::WriteAhead, ObjectLevel::None, 100, 1);
    let _ = writer.key_seed();
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

    let len = bytes.len();
    let trailer_start = len - TRAILER_LEN;
    let list_off = usize::try_from(u64::from_le_bytes(
        bytes[trailer_start..trailer_start + 8]
            .try_into()
            .expect("u64"),
    ))
    .expect("usize");
    let list_len = usize::try_from(u64::from_le_bytes(
        bytes[trailer_start + 8..trailer_start + 16]
            .try_into()
            .expect("u64"),
    ))
    .expect("usize");
    let list_uncompressed = usize::try_from(u64::from_le_bytes(
        bytes[trailer_start + 16..trailer_start + 24]
            .try_into()
            .expect("u64"),
    ))
    .expect("usize");
    let mut raw_list =
        zstd::bulk::decompress(&bytes[list_off..list_off + list_len], list_uncompressed)
            .expect("decompress list");
    raw_list[120 + 32..120 + 40].copy_from_slice(&(OBJECT_HEADER_LEN as u64).to_le_bytes());
    let recompressed = zstd::bulk::compress(&raw_list, DEFAULT_ZSTD_LEVEL).expect("recompress");
    let mut overlapping = bytes[..list_off].to_vec();
    overlapping.extend_from_slice(&recompressed);
    let header = super::parse_object_header(&bytes).expect("header");
    let new_sum = part_list_checksum(&header, &recompressed);
    overlapping.extend_from_slice(&(list_off as u64).to_le_bytes());
    overlapping.extend_from_slice(&(recompressed.len() as u64).to_le_bytes());
    overlapping.extend_from_slice(&(raw_list.len() as u64).to_le_bytes());
    overlapping.extend_from_slice(&new_sum.to_le_bytes());
    overlapping.extend_from_slice(b"USCOPEOE");
    assert!(matches!(
        ObjectReader::open(&overlapping),
        Err(ObjectError::Layout(_))
    ));
}

#[test]
fn decode_tables_enforces_cumulative_decompression_budget() {
    let mut oversized = vec![b'a'; 33 * 1024 * 1024];
    oversized.push(0);
    let mut sw_over = SectionWriter::new(
        SectionKind::Strings,
        super::TABLE_VERSION,
        TimeUnit::Ns,
        1,
        0,
        DEFAULT_ZSTD_LEVEL,
    );
    sw_over
        .add_column(WHOLE_SECTION_GROUP, 0, Coding::Bytes, 0, &oversized)
        .expect("oversized col");
    assert!(matches!(sw_over.finish(0), Err(ObjectError::Limit(_))));

    let mut strings = vec![b'a'; 24 * 1024 * 1024];
    strings.push(0);
    let rows = 200_000_u32;
    let varint_zeros = vec![0_u8; rows as usize];
    let mut sw = SectionWriter::new(
        SectionKind::Strings,
        super::TABLE_VERSION,
        TimeUnit::Ns,
        1,
        0,
        DEFAULT_ZSTD_LEVEL,
    );
    sw.add_column(WHOLE_SECTION_GROUP, 0, Coding::Bytes, 0, &strings)
        .expect("strings col");
    let mut sections = vec![sw.finish(0).expect("strings section")];
    for (kind, u32_cols, u8_cols) in [
        (SectionKind::Lines, 4_u8, 0_u8),
        (SectionKind::Functions, 4, 1),
        (SectionKind::Sites, 4, 1),
    ] {
        let mut sw = SectionWriter::new(
            kind,
            super::TABLE_VERSION,
            TimeUnit::Ns,
            rows,
            0,
            DEFAULT_ZSTD_LEVEL,
        );
        for id in 0..u32_cols {
            sw.add_column(
                WHOLE_SECTION_GROUP,
                id,
                Coding::VarintPlain,
                0,
                &varint_zeros,
            )
            .expect("u32 col");
        }
        for offset in 0..u8_cols {
            sw.add_column(
                WHOLE_SECTION_GROUP,
                u32_cols + offset,
                Coding::Bytes,
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
        super::decode_tables_budgeted(&raw_sections, 32 * 1024 * 1024),
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
        super::TABLE_VERSION,
        TimeUnit::Ns,
        1,
        0,
        DEFAULT_ZSTD_LEVEL,
    );
    sw.add_column(WHOLE_SECTION_GROUP, 0, Coding::Bytes, 0, &strings)
        .expect("strings col");
    let strings_sec = sw.finish(0).expect("strings section");

    let rows = 80_u32;
    let zeros = vec![0_u8; rows as usize];
    let ones = vec![1_u8; rows as usize];
    let mut dw = SectionWriter::new(
        SectionKind::Dimensions,
        super::TABLE_VERSION,
        TimeUnit::Ns,
        rows,
        0,
        DEFAULT_ZSTD_LEVEL,
    );
    dw.add_column(WHOLE_SECTION_GROUP, 0, Coding::VarintPlain, 0, &zeros)
        .expect("proc col");
    dw.add_column(WHOLE_SECTION_GROUP, 1, Coding::VarintPlain, 0, &ones)
        .expect("name col");
    dw.add_column(WHOLE_SECTION_GROUP, 2, Coding::VarintPlain, 0, &ones)
        .expect("val col");
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

#[test]
fn verbatim_resanitize_detects_secrets_in_syscall_capture_blobs() {
    use super::RawSection;
    use crate::sanitize::re_sanitize_sections_verbatim;

    let sanitizer = Sanitizer::new(&Builtin::DEFAULT, Vec::new(), HashKey::new(b"test-key"));
    let secret_payload = b"Authorization: Bearer ghp_123456789012345678901234567890123456  ";
    assert_eq!(secret_payload.len() % 8, 0);

    let mut builder = CaptureBuilder::new();
    let proc_id = builder.process(Pid::new(10), 1_000);
    builder.process_name(proc_id, "app");
    let thread_id = builder.thread(Pid::new(10), Tid::new(10), 1_000);
    builder.thread_name(thread_id, "main");

    let mut cap = builder.finish();
    let mut blob = Vec::new();
    blob.extend_from_slice(&[1_u8, 0, 0, 0]);
    blob.extend_from_slice(&0_u32.to_le_bytes());
    blob.extend_from_slice(
        &u32::try_from(secret_payload.len())
            .expect("u32")
            .to_le_bytes(),
    );
    blob.extend_from_slice(secret_payload);
    let cap_len = u32::try_from(blob.len()).expect("u32");
    let ret_len = i64::try_from(secret_payload.len()).expect("i64");
    cap.syscall_blobs = blob;
    cap.syscalls.push(Syscall {
        entered: 1_100.into(),
        exited: 1_200.into(),
        ret: ret_len.into(),
        args: [
            1.into(),
            0.into(),
            (secret_payload.len() as u64).into(),
            0.into(),
            0.into(),
            0.into(),
        ],
        thread: thread_id.into(),
        number: 1.into(),
        stack: NONE.into(),
        captures: 0.into(),
        captures_len: cap_len.into(),
        abi: 0,
        outcome: 0,
        flags: 0,
        reserved: 0,
    });

    let data = ChunkData {
        capture: cap,
        chunks: vec![ChunkRow {
            agent: 1,
            number: 1,
            start: 1_000,
            end: 2_000,
            sampled_ns: 0,
            preset: 1,
            time_unit: TimeUnit::Ns,
            policy: 1,
            rules: String::new(),
        }],
        dimensions: Vec::new(),
        sanitizer_counts: Vec::new(),
    };
    let encoded = encode_tables(&data, TimeUnit::Ns, DEFAULT_ZSTD_LEVEL, 0).expect("encode");
    let raw: Vec<RawSection<'_>> = encoded
        .iter()
        .map(|s| RawSection {
            kind: s.kind,
            rows: s.rows,
            key: s.key,
            checksum: 0,
            bytes: &s.bytes,
        })
        .collect();
    let (resanitized, verbatim) =
        re_sanitize_sections_verbatim(&raw, &sanitizer, DEFAULT_ZSTD_LEVEL).expect("resanitize");
    assert!(
        !verbatim,
        "secret in Words-coded syscall capture blob must trigger rewrite"
    );
    let raw_after: Vec<RawSection<'_>> = resanitized
        .iter()
        .map(|s| RawSection {
            kind: s.kind,
            rows: s.rows,
            key: s.key,
            checksum: 0,
            bytes: &s.bytes,
        })
        .collect();
    let decoded = decode_tables(&raw_after).expect("decode after rewrite");
    assert!(
        !decoded
            .capture
            .syscall_blobs
            .windows(b"ghp_12345678".len())
            .any(|w| w == b"ghp_12345678")
    );
}

#[test]
fn verbatim_resanitize_detects_secrets_in_syscall_note_blobs() {
    use super::RawSection;
    use crate::sanitize::re_sanitize_sections_verbatim;

    let sanitizer = Sanitizer::new(&Builtin::DEFAULT, Vec::new(), HashKey::new(b"test-key"));
    let mut builder2 = CaptureBuilder::new();
    let proc2 = builder2.process(Pid::new(20), 1_000);
    builder2.process_name(proc2, "app2");
    let thread2 = builder2.thread(Pid::new(20), Tid::new(20), 1_000);
    builder2.thread_name(thread2, "main");
    let mut cap2 = builder2.finish();
    let note_secret = b"token=ghp_123456789012345678901234567890123456";
    cap2.syscall_blobs = note_secret.to_vec();
    cap2.syscall_notes.push(SyscallNote {
        time: 1_500.into(),
        value: 0.into(),
        extra: 0.into(),
        thread: thread2.into(),
        process: proc2.into(),
        text: NONE.into(),
        stack: NONE.into(),
        blob: 0.into(),
        blob_len: u32::try_from(note_secret.len()).expect("u32").into(),
        calls: 0.into(),
        kind: 1,
        flags: 0,
        reserved: [0; 10],
    });
    let data2 = ChunkData {
        capture: cap2,
        chunks: vec![ChunkRow {
            agent: 1,
            number: 1,
            start: 1_000,
            end: 2_000,
            sampled_ns: 0,
            preset: 1,
            time_unit: TimeUnit::Ns,
            policy: 1,
            rules: String::new(),
        }],
        dimensions: Vec::new(),
        sanitizer_counts: Vec::new(),
    };
    let encoded2 = encode_tables(&data2, TimeUnit::Ns, DEFAULT_ZSTD_LEVEL, 0).expect("encode2");
    let raw2: Vec<RawSection<'_>> = encoded2
        .iter()
        .map(|s| RawSection {
            kind: s.kind,
            rows: s.rows,
            key: s.key,
            checksum: 0,
            bytes: &s.bytes,
        })
        .collect();
    let (resanitized2, verbatim2) =
        re_sanitize_sections_verbatim(&raw2, &sanitizer, DEFAULT_ZSTD_LEVEL).expect("resanitize2");
    assert!(
        !verbatim2,
        "secret in syscall note blob must trigger rewrite"
    );
    let raw_after2: Vec<RawSection<'_>> = resanitized2
        .iter()
        .map(|s| RawSection {
            kind: s.kind,
            rows: s.rows,
            key: s.key,
            checksum: 0,
            bytes: &s.bytes,
        })
        .collect();
    let decoded2 = decode_tables(&raw_after2).expect("decode2");
    assert!(
        !decoded2
            .capture
            .syscall_blobs
            .windows(b"ghp_12345678".len())
            .any(|w| w == b"ghp_12345678")
    );
}

#[test]
fn large_syscall_blobs_over_32_mib_page_and_round_trip_in_usprof() {
    let mut builder = CaptureBuilder::new();
    let proc_id = builder.process(Pid::new(1), 1_000);
    builder.process_name(proc_id, "big-blobs");
    let thread_id = builder.thread(Pid::new(1), Tid::new(1), 1_000);
    builder.thread_name(thread_id, "main");
    let mut cap = builder.finish();

    let chunk_payload = vec![b'A'; 100 * 1024];
    let call_count = 330_usize;
    cap.syscall_blobs
        .reserve(call_count * (chunk_payload.len() + 12));
    for i in 0..call_count {
        let start = u32::try_from(cap.syscall_blobs.len()).expect("u32");
        cap.syscall_blobs.extend_from_slice(&[1_u8, 0, 0, 0]);
        cap.syscall_blobs.extend_from_slice(&0_u32.to_le_bytes());
        cap.syscall_blobs.extend_from_slice(
            &u32::try_from(chunk_payload.len())
                .expect("u32")
                .to_le_bytes(),
        );
        cap.syscall_blobs.extend_from_slice(&chunk_payload);
        let len = u32::try_from(cap.syscall_blobs.len()).expect("u32") - start;
        let idx_i64 = i64::try_from(i).expect("i64");
        let ret_i64 = i64::try_from(chunk_payload.len()).expect("i64");
        cap.syscalls.push(Syscall {
            entered: (1_000 + idx_i64 * 10).into(),
            exited: (1_005 + idx_i64 * 10).into(),
            ret: ret_i64.into(),
            args: [
                1.into(),
                0.into(),
                (chunk_payload.len() as u64).into(),
                0.into(),
                0.into(),
                0.into(),
            ],
            thread: thread_id.into(),
            number: 1.into(),
            stack: NONE.into(),
            captures: start.into(),
            captures_len: len.into(),
            abi: 0,
            outcome: 0,
            flags: 0,
            reserved: 0,
        });
    }

    let mut encoded_bytes = Vec::new();
    crate::capture::file::write(&cap, &mut encoded_bytes)
        .expect("write >32 MiB syscall_blobs capture");
    let reopened =
        crate::capture::file::read(&encoded_bytes).expect("reopen >32 MiB syscall_blobs capture");
    assert_eq!(reopened.capture.syscalls.len(), cap.syscalls.len());
    assert_eq!(
        reopened.capture.syscall_blobs.len(),
        cap.syscall_blobs.len()
    );
    assert_eq!(reopened.capture.syscall_blobs, cap.syscall_blobs);
}

#[test]
fn quantized_page_bounds_match_decoded_timestamps() {
    let mut builder = CaptureBuilder::new();
    let proc_id = builder.process(Pid::new(1), 1_000);
    builder.process_name(proc_id, "q");
    let thread_id = builder.thread(Pid::new(1), Tid::new(1), 1_000);
    builder.thread_name(thread_id, "main");
    let frame = builder.frame(FrameKind::Native, NONE, 0x1000, 0);
    let stack = builder.stack(&[frame]);
    let total = super::DEFAULT_PAGE_ROWS + 8;
    for i in 0..total {
        let ts = 1_999_i64 + i64::try_from(i).expect("i64") * 1_000;
        builder.sample(thread_id, ts, 100, stack, NONE);
        builder.off_cpu(thread_id, ts, 100, false, 0);
    }
    let cap = builder.finish();
    let data = ChunkData {
        capture: cap,
        chunks: Vec::new(),
        dimensions: Vec::new(),
        sanitizer_counts: Vec::new(),
    };
    let sections = encode_tables(&data, TimeUnit::Us, DEFAULT_ZSTD_LEVEL, 0).expect("encode");

    let samples_sec = sections
        .iter()
        .find(|sec| sec.kind == SectionKind::Samples)
        .expect("samples");
    let reader = SectionReader::open(&samples_sec.bytes, SectionKind::Samples).expect("open");
    assert_eq!(reader.page_count(), 2);
    assert_eq!(reader.pages_overlapping(1_000, 1_000), vec![0]);

    let offcpu_sec = sections
        .iter()
        .find(|sec| sec.kind == SectionKind::OffCpu)
        .expect("offcpu");
    let off_reader = SectionReader::open(&offcpu_sec.bytes, SectionKind::OffCpu).expect("open");
    assert_eq!(off_reader.page_count(), 2);
    assert_eq!(off_reader.pages_overlapping(1_000, 1_000), vec![0]);
}

#[test]
fn event_bundle_preserves_machine_local_process_subset() {
    use super::RawSection;

    let mut builder = CaptureBuilder::new();
    let p0 = builder.process(Pid::new(100), 1_000);
    builder.process_name(p0, "alpha");
    let p1 = builder.process(Pid::new(200), 1_000);
    builder.process_name(p1, "beta-idle");
    let p2 = builder.process(Pid::new(300), 1_000);
    builder.process_name(p2, "gamma-other-machine");
    let t0 = builder.thread(Pid::new(100), Tid::new(100), 1_000);
    builder.thread_name(t0, "main");
    let frame = builder.frame(FrameKind::Native, NONE, 0x1000, 0);
    let stack = builder.stack(&[frame]);
    builder.sample(t0, 2_000, 500, stack, NONE);
    let shared_cap = builder.finish();

    let mut local_builder = CaptureBuilder::new();
    let lp0 = local_builder.process(Pid::new(100), 1_000);
    local_builder.process_name(lp0, "alpha");
    let lp1 = local_builder.process(Pid::new(200), 1_000);
    local_builder.process_name(lp1, "beta-idle");
    let lt0 = local_builder.thread(Pid::new(100), Tid::new(100), 1_000);
    local_builder.thread_name(lt0, "main");
    let lframe = local_builder.frame(FrameKind::Native, NONE, 0x1000, 0);
    let lstack = local_builder.stack(&[lframe]);
    local_builder.sample(lt0, 2_000, 500, lstack, NONE);
    let local_cap = local_builder.finish();

    let data = ChunkData {
        capture: local_cap,
        chunks: Vec::new(),
        dimensions: vec![DimensionRow {
            process: 1,
            name: "service.name".into(),
            value: "idle-svc".into(),
        }],
        sanitizer_counts: Vec::new(),
    };
    let mut shared_strings = shared_cap.strings.clone();
    for extra in ["service.name", "idle-svc"] {
        shared_strings.extend_from_slice(extra.as_bytes());
        shared_strings.push(0);
    }

    let bundle_secs = super::encode_event_bundle(
        &data,
        &shared_strings,
        &[0, 1],
        TimeUnit::Ns,
        DEFAULT_ZSTD_LEVEL,
        7,
    )
    .expect("encode");
    let raw: Vec<RawSection<'_>> = bundle_secs
        .iter()
        .map(|s| RawSection {
            kind: s.kind,
            rows: s.rows,
            key: s.key,
            checksum: 0,
            bytes: &s.bytes,
        })
        .collect();
    let shared = super::SharedSymbols {
        strings: &shared_strings,
        modules: &shared_cap.modules,
        processes: &shared_cap.processes,
        frames: &shared_cap.frames,
        frame_symbols: &shared_cap.frame_symbols,
        lines: &shared_cap.lines,
        functions: &shared_cap.functions,
        stacks: &shared_cap.stacks,
        sites: &shared_cap.sites,
    };

    let decoded = super::decode_event_bundle(&raw, &shared, false).expect("decode");
    assert_eq!(decoded.capture.processes.len(), 2);
    assert_eq!(decoded.capture.processes[0].pid.get(), 100);
    assert_eq!(decoded.capture.processes[1].pid.get(), 200);
    assert_eq!(decoded.capture.threads[0].process.get(), 0);
    assert_eq!(decoded.dimensions[0].process, 1);
}

#[test]
fn verbatim_resanitize_detects_leading_whitespace_json_argv() {
    use super::RawSection;
    use crate::sanitize::re_sanitize_sections_verbatim;

    let sanitizer = Sanitizer::new(&Builtin::DEFAULT, Vec::new(), HashKey::new(b"test-key"));
    let mut builder = CaptureBuilder::new();
    builder.setting(
        "arguments",
        "  [\"/usr/bin/app\", \"--password\", \"hunter2\"]",
    );
    let _proc = builder.process(Pid::new(1), 1_000);
    let data = ChunkData {
        capture: builder.finish(),
        chunks: vec![ChunkRow {
            agent: 1,
            number: 1,
            start: 1_000,
            end: 2_000,
            sampled_ns: 0,
            preset: 0,
            time_unit: TimeUnit::Ns,
            policy: 1,
            rules: String::new(),
        }],
        dimensions: Vec::new(),
        sanitizer_counts: Vec::new(),
    };
    let encoded = encode_tables(&data, TimeUnit::Ns, DEFAULT_ZSTD_LEVEL, 0).expect("encode");
    let raw: Vec<RawSection<'_>> = encoded
        .iter()
        .map(|s| RawSection {
            kind: s.kind,
            rows: s.rows,
            key: s.key,
            checksum: 0,
            bytes: &s.bytes,
        })
        .collect();
    let (resanitized, verbatim) =
        re_sanitize_sections_verbatim(&raw, &sanitizer, DEFAULT_ZSTD_LEVEL).expect("resanitize");
    assert!(
        !verbatim,
        "leading whitespace argv JSON must trigger rewrite"
    );
    let raw_after: Vec<RawSection<'_>> = resanitized
        .iter()
        .map(|s| RawSection {
            kind: s.kind,
            rows: s.rows,
            key: s.key,
            checksum: 0,
            bytes: &s.bytes,
        })
        .collect();
    let decoded = decode_tables(&raw_after).expect("decode");
    assert!(
        !decoded
            .capture
            .strings
            .windows(b"hunter2".len())
            .any(|w| w == b"hunter2")
    );
}

#[test]
fn encode_post_rejects_compressed_tables_exceeding_decode_budget() {
    let mut builder = CaptureBuilder::new();
    let proc_id = builder.process(Pid::new(1), 1_000);
    let mut cap = builder.finish();
    cap.syscall_stats.resize(
        700_000,
        SyscallStat {
            calls: 1.into(),
            errors: 0.into(),
            timed: 0.into(),
            total_ns: 0.into(),
            min_ns: 0.into(),
            max_ns: 0.into(),
            bytes: 0.into(),
            log2_ns: [0.into(); crate::syscalls::stats::BUCKETS],
            process: proc_id.into(),
            thread: NONE.into(),
            number: 1.into(),
            errnos: 0.into(),
            errno_count: 0.into(),
            abi: 0,
            reserved: [0; 3],
        },
    );
    let policy = Sanitizer::new(&Builtin::DEFAULT, Vec::new(), HashKey::new(b"test-key"));
    let sanitized = sanitize_chunk(
        ChunkData {
            capture: cap,
            chunks: vec![ChunkRow {
                agent: 1,
                number: 1,
                start: 1_000,
                end: 2_000,
                sampled_ns: 0,
                preset: 0,
                time_unit: TimeUnit::Ns,
                policy: 1,
                rules: String::new(),
            }],
            dimensions: Vec::new(),
            sanitizer_counts: Vec::new(),
        },
        TimeUnit::Ns,
        &policy,
    );
    assert!(matches!(
        encode_post(&[sanitized], 1, 2_000, DEFAULT_ZSTD_LEVEL),
        Err(ObjectError::Limit(_))
    ));
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "verifies legacy missing-section fallback and explicit empty process membership"
)]
fn event_bundle_without_process_membership_section_remains_compatible() {
    use super::{RawSection, SharedSymbols, decode_event_bundle, encode_event_bundle};

    let mut b = CaptureBuilder::new();
    let p0 = b.process(Pid::new(100), 1_000);
    b.process_name(p0, "alpha");
    let p1 = b.process(Pid::new(200), 1_000);
    b.process_name(p1, "beta");
    let t1 = b.thread(Pid::new(200), Tid::new(201), 1_000);
    let f = b.frame(FrameKind::Native, NONE, 0x1000, 0);
    let s = b.stack(&[f]);
    b.sample(t1, 1_500, 10, s, NONE);
    let cap = b.finish();

    let shared = SharedSymbols {
        strings: &cap.strings,
        modules: &cap.modules,
        processes: &cap.processes,
        frames: &cap.frames,
        frame_symbols: &cap.frame_symbols,
        lines: &cap.lines,
        functions: &cap.functions,
        stacks: &cap.stacks,
        sites: &cap.sites,
    };
    let data = ChunkData {
        capture: cap.clone(),
        chunks: vec![ChunkRow {
            agent: 1,
            number: 1,
            start: 1_000,
            end: 2_000,
            sampled_ns: 1_000,
            preset: 0,
            time_unit: TimeUnit::Ns,
            policy: 0,
            rules: String::new(),
        }],
        dimensions: Vec::new(),
        sanitizer_counts: Vec::new(),
    };

    let encoded = encode_event_bundle(
        &data,
        &cap.strings,
        &[p1],
        TimeUnit::Ns,
        DEFAULT_ZSTD_LEVEL,
        7,
    )
    .expect("encode bundle");
    let raw_without_processes: Vec<RawSection<'_>> = encoded
        .iter()
        .filter(|sec| sec.kind != SectionKind::Processes)
        .map(|sec| RawSection {
            kind: sec.kind,
            rows: sec.rows,
            key: sec.key,
            checksum: 0,
            bytes: &sec.bytes,
        })
        .collect();

    let decoded =
        decode_event_bundle(&raw_without_processes, &shared, false).expect("legacy bundle decode");
    assert_eq!(decoded.capture.processes.len(), 2);
    assert_eq!(decoded.capture.threads.len(), 1);
    assert_eq!(decoded.capture.threads[0].process.get(), p1);

    let empty_data = ChunkData {
        capture: CaptureBuilder::new().finish(),
        chunks: vec![ChunkRow {
            agent: 2,
            number: 1,
            start: 1_000,
            end: 2_000,
            sampled_ns: 0,
            preset: 0,
            time_unit: TimeUnit::Ns,
            policy: 0,
            rules: String::new(),
        }],
        dimensions: Vec::new(),
        sanitizer_counts: Vec::new(),
    };
    let encoded_empty = encode_event_bundle(
        &empty_data,
        &cap.strings,
        &[],
        TimeUnit::Ns,
        DEFAULT_ZSTD_LEVEL,
        8,
    )
    .expect("encode empty bundle");
    assert!(
        encoded_empty
            .iter()
            .any(|sec| sec.kind == SectionKind::Processes && sec.rows == 0)
    );
    let raw_empty: Vec<RawSection<'_>> = encoded_empty
        .iter()
        .map(|sec| RawSection {
            kind: sec.kind,
            rows: sec.rows,
            key: sec.key,
            checksum: 0,
            bytes: &sec.bytes,
        })
        .collect();
    let decoded_empty =
        decode_event_bundle(&raw_empty, &shared, false).expect("decode empty bundle");
    assert!(decoded_empty.capture.processes.is_empty());
}

proptest! {
    #[test]
    fn prop_sparse_and_dense_delta_by_key_round_trip(
        pairs in proptest::collection::vec(
            (prop_oneof![0_u32..16, 1_048_576_u32..2_000_000, (u32::MAX - 100)..=u32::MAX], any::<u64>()),
            0..128
        )
    ) {
        let keys: Vec<u32> = pairs.iter().map(|&(k, _)| k).collect();
        let vals: Vec<u64> = pairs.iter().map(|&(_, v)| v).collect();
        let encoded = delta_by_key_encode_u64(&vals, &keys);
        let decoded = delta_by_key_decode_u64(&encoded, &keys).expect("decode");
        prop_assert_eq!(decoded, vals);
    }

    #[test]
    fn prop_u64_codecs_round_trip_extremes(
        vals in proptest::collection::vec(
            prop_oneof![
                Just(0_u64),
                Just(1_u64),
                Just(u64::MAX - 1),
                Just(u64::MAX),
                0_u64..1024,
                any::<u64>(),
            ],
            0..96
        )
    ) {
        let budget = DecodeBudget::new(2 << 20);

        let v = encode_varints(&vals);
        prop_assert_eq!(&decode_varints(&v, vals.len()).expect("varint"), &vals);

        let d = delta_encode_u64(&vals);
        prop_assert_eq!(&delta_decode_u64(&d), &vals);

        let bases: Vec<u64> = vals.iter().map(|&x| x.wrapping_sub(7)).collect();
        let r = super::codec::relative_encode_u64(&vals, &bases);
        prop_assert_eq!(&super::codec::relative_decode_u64(&r, &bases).expect("rel"), &vals);

        let dict = encode_dict_u64(&vals);
        prop_assert_eq!(
            &super::codec::decode_dict_u64_budgeted(&dict, vals.len(), Some(&budget)).expect("dict"),
            &vals
        );

        let nz = encode_nonzero_u64(&vals);
        prop_assert_eq!(&decode_nonzero_u64(&nz, vals.len()).expect("nonzero"), &vals);

        let recs: Vec<[u8; 8]> = vals.iter().map(|&x| x.to_le_bytes()).collect();
        let slices: Vec<&[u8]> = recs.iter().map(<[u8; 8]>::as_slice).collect();
        let enc_words = encode_words(&slices, 8);
        let dec_words = decode_words(&enc_words, recs.len(), 8).expect("words");
        let expected_words: Vec<u8> = recs.iter().flat_map(|r| *r).collect();
        prop_assert_eq!(dec_words, expected_words);
    }

    #[test]
    fn prop_sparse_histograms_and_section_index_round_trip(
        hists in proptest::collection::vec(
            proptest::collection::vec((0_usize..40, prop_oneof![1_u64..64, any::<u64>()]), 0..8),
            0..24
        ),
        metas in proptest::collection::vec(
            (0_u16..32, any::<u32>(), 0_u64..1_000_000, any::<u32>(), any::<u64>(), any::<u64>()),
            0..32
        )
    ) {
        let mut arrays = Vec::with_capacity(hists.len());
        for entries in hists {
            let mut arr = [0_u64; 40];
            for (b, c) in entries {
                arr[b] = c;
            }
            arrays.push(arr);
        }
        let enc_h = encode_sparse_histograms(&arrays);
        let budget = DecodeBudget::new(1 << 20);
        let dec_h = decode_sparse_histograms(&enc_h, arrays.len(), Some(&budget)).expect("hist");
        prop_assert_eq!(dec_h, arrays);

        let sections: Vec<SectionMeta> = metas
            .into_iter()
            .filter_map(|(k, rows, offset, stored_len, key, checksum)| {
                SectionKind::from_number(k).map(|kind| SectionMeta {
                    kind: kind as u16,
                    rows,
                    offset,
                    stored_len,
                    key,
                    checksum,
                })
            })
            .collect();
        let enc_idx = encode_section_index(7, &sections);
        let (part_idx, dec_idx) = decode_section_index(&enc_idx).expect("section index");
        prop_assert_eq!(part_idx, 7);
        prop_assert_eq!(dec_idx, sections);
    }

    #[test]
    fn prop_bp_tree_forest_round_trips_arbitrary_parents(
        node_count in 0_usize..64,
        parent_choices in proptest::collection::vec(0_u32..128, 0..64),
    ) {
        let mut parents = Vec::with_capacity(node_count);
        for i in 0..node_count {
            let choice = parent_choices.get(i).copied().unwrap_or(0);
            if i == 0 || choice % 4 == 0 {
                parents.push(NONE);
            } else {
                parents.push(choice % u32::try_from(i).expect("idx"));
            }
        }
        let (dfs_parents, old_to_dfs, dfs_to_old) = forest_to_dfs(&parents);
        prop_assert_eq!(dfs_parents.len(), node_count);
        prop_assert_eq!(dfs_to_old.len(), node_count);
        let bp_bytes = encode_bp_tree(&dfs_parents);
        let budget = DecodeBudget::new(1 << 20);
        let decoded = super::codec::decode_bp_tree_budgeted(&bp_bytes, node_count, Some(&budget)).expect("bp");
        prop_assert_eq!(&decoded, &dfs_parents);
        for (dfs_idx, &orig_idx) in dfs_to_old.iter().enumerate() {
            let orig_parent = parents[orig_idx];
            let expected = if orig_parent == NONE {
                NONE
            } else {
                old_to_dfs[orig_parent as usize]
            };
            prop_assert_eq!(dfs_parents[dfs_idx], expected);
        }

        if node_count >= 2 {
            let mut cyclic = parents;
            cyclic[0] = 1;
            cyclic[1] = 0;
            let (cyc_dfs, _, _) = forest_to_dfs(&cyclic);
            let cyc_bp = encode_bp_tree(&cyc_dfs);
            prop_assert_eq!(
                super::codec::decode_bp_tree_budgeted(&cyc_bp, node_count, Some(&budget)).expect("cyc bp"),
                cyc_dfs
            );
        }
    }

    #[test]
    fn prop_section_writer_multi_page_and_narrow_decoders_agree(
        pages in proptest::collection::vec(
            proptest::collection::vec(any::<u8>(), 1..32),
            1..5
        )
    ) {
        let total_rows: usize = pages.iter().map(Vec::len).sum();
        let mut sw = SectionWriter::new(
            SectionKind::Samples,
            super::TABLE_VERSION,
            TimeUnit::Ns,
            u32::try_from(total_rows).expect("rows"),
            0,
            DEFAULT_ZSTD_LEVEL,
        );
        let mut key_base = 100_i64;
        for vals in &pages {
            let u64s: Vec<u64> = vals.iter().map(|&b| u64::from(b)).collect();
            sw.add_u64_smart(WHOLE_SECTION_GROUP, 0, &u64s, false).expect("add col");
            sw.finish_page(
                key_base,
                key_base + i64::try_from(vals.len()).expect("len"),
                u32::try_from(vals.len()).expect("len"),
            )
            .expect("finish page");
            key_base += 1000;
        }
        let encoded = sw.finish_paged(0).expect("finish paged");
        let budget = DecodeBudget::new(1 << 20);
        let reader = SectionReader::open_budgeted(&encoded.bytes, SectionKind::Samples, &budget)
            .expect("open section");
        prop_assert_eq!(reader.rows as usize, total_rows);
        prop_assert_eq!(reader.page_count(), pages.len());
        prop_assert_eq!(reader.page_entry(0).unwrap().min_key.get(), 100);
        if pages.len() == 1 {
            prop_assert!(reader.read_single_page().is_ok());
        } else {
            prop_assert!(reader.read_single_page().is_err());
        }
        for (idx, expected) in pages.iter().enumerate() {
            let page = reader.read_page(idx).expect("read page");
            let col = page.column(WHOLE_SECTION_GROUP, 0).expect("col 0");
            let as_u64 = col.decode_u64(expected.len(), Some(&budget)).expect("u64");
            let as_u32 = col.decode_u32(expected.len(), Some(&budget)).expect("u32");
            let as_u8 = col.decode_u8(expected.len(), Some(&budget)).expect("u8");
            prop_assert_eq!(as_u8, expected.clone());
            prop_assert_eq!(as_u32, expected.iter().map(|&b| u32::from(b)).collect::<Vec<_>>());
            prop_assert_eq!(as_u64, expected.iter().map(|&b| u64::from(b)).collect::<Vec<_>>());
        }
    }
}

#[test]
fn object_header_tamper_is_detected_and_project_mismatch_rejected() {
    let capture = build_rich_capture();
    let data = ChunkData {
        capture,
        chunks: vec![ChunkRow {
            agent: 7,
            number: 1,
            start: 1_000_000_000,
            end: 1_005_000_000,
            sampled_ns: 1_000,
            preset: 0,
            time_unit: TimeUnit::Us,
            policy: 1,
            rules: String::new(),
        }],
        dimensions: vec![DimensionRow {
            process: 0,
            name: "service.name".into(),
            value: "api".into(),
        }],
        sanitizer_counts: Vec::new(),
    };
    let sections = encode_tables(&data, TimeUnit::Us, DEFAULT_ZSTD_LEVEL, 1).expect("encode");
    let mut writer = ObjectWriter::new(ObjectKind::Post, ObjectLevel::None, 1_005_000_000, 7);
    writer
        .add_part(&PartSpec {
            org_id: 42,
            project_id: 99,
            starts_at: 1_000_000_000,
            ends_at: 1_005_000_000,
            signal: Signal::Profile,
            part: PartKind::Chunks,
            sections,
        })
        .expect("add part");
    let (bytes, _) = writer.finish().expect("finish");

    for offset in 0..OBJECT_HEADER_LEN {
        let mut tampered = bytes.clone();
        tampered[offset] ^= 0x01;
        assert!(
            ObjectReader::open(&tampered).is_err(),
            "header byte {offset} tamper must fail verification"
        );
    }

    let reader = ObjectReader::open(&bytes).expect("open valid");
    assert!(matches!(
        reader.part_sections_checked(0, Some(42), Some(100)),
        Err(ObjectError::ProjectMismatch {
            expected: 100,
            found: 99
        })
    ));
    assert!(reader.part_sections_checked(0, Some(42), Some(99)).is_ok());
}

#[test]
fn tables_round_trip_all_time_units_and_merge_chunks_preserves_duplicates() {
    let mut b1 = CaptureBuilder::new();
    let p1 = b1.process(Pid::new(10), 1_000_000);
    let t1 = b1.thread(Pid::new(10), Tid::new(10), 1_000_000);
    let m1 = b1.module("/bin/a", None, [1; 16], 4096);
    let f1 = b1.frame(FrameKind::Native, m1, 0x100, 0);
    let s1 = b1.stack(&[f1]);
    b1.sample(t1, 2_000_000, 10_000, s1, NONE);
    let dup1 = b1.string("duplicate");
    let dup2 = b1.string("duplicate");
    assert_eq!(dup1, dup2);
    let mut cap1 = b1.finish();
    cap1.strings.extend_from_slice(b"duplicate\0");

    for unit in [TimeUnit::Ns, TimeUnit::Us, TimeUnit::Ms] {
        let chunk = ChunkData {
            capture: cap1.clone(),
            chunks: vec![ChunkRow {
                agent: 1,
                number: 1,
                start: 1_000_000,
                end: 3_000_000,
                sampled_ns: 10_000,
                preset: 0,
                time_unit: unit,
                policy: 1,
                rules: "r1".into(),
            }],
            dimensions: vec![DimensionRow {
                process: p1,
                name: "service.name".into(),
                value: "svc".into(),
            }],
            sanitizer_counts: vec![SanitizerCountRow {
                rule: "r1".into(),
                field: FieldClass::ZoneText.number(),
                fired: u64::MAX - 1,
            }],
        };
        let enc = encode_tables(&chunk, unit, DEFAULT_ZSTD_LEVEL, 5).expect("encode");
        let raw: Vec<super::RawSection<'_>> = enc
            .iter()
            .map(|s| super::RawSection {
                kind: s.kind,
                rows: s.rows,
                key: s.key,
                checksum: 0,
                bytes: &s.bytes,
            })
            .collect();
        let dec = decode_tables(&raw).expect("decode");
        assert_eq!(dec.capture.samples.len(), 1);
        assert_eq!(dec.chunks[0].start, 1_000_000);

        let merged = merge_chunks(&[chunk.clone(), chunk]).expect("merge");
        assert_eq!(merged.capture.samples.len(), 2);
        assert_eq!(merged.sanitizer_counts[0].fired, u64::MAX);
    }
}
