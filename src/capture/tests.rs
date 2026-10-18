use super::builder::{InterpretedPlace, LineInput};
use super::*;
use crate::model::{Pid, Tid};

/// A small capture with two threads, shared stacks, symbols, and a zone.
fn sample_capture(offset: i64) -> Capture {
    let mut builder = CaptureBuilder::new();
    builder.setting("start", &offset.to_string());
    builder.setting("end", &(offset + 100).to_string());
    let module = builder.module("/bin/app", Some("abcd"), [7; 16], 4096);
    let main_thread = builder.thread(Pid::new(10), Tid::new(10), offset);
    let worker = builder.thread(Pid::new(10), Tid::new(11), offset + 1);
    builder.thread_name(worker, "worker");
    let main = builder.frame(FrameKind::Native, module, 0x1000, 0);
    let work = builder.frame(FrameKind::Native, module, 0x2000, 0);
    let cut = builder.truncation(Truncation::FramePointerNotKept, module);
    builder.symbolize(
        main,
        module,
        &[LineInput {
            function: "main",
            file: Some("app.c"),
            line: 3,
            column: 1,
            declared: Some(("app.c", 1)),
            role: 0,
        }],
    );
    let handle = builder.interpreted(&InterpretedPlace {
        function: "Server.handle",
        file: Some("server.py"),
        line: 12,
        declared: Some(10),
    });
    let deep = builder.stack(&[work, main]);
    let shallow = builder.stack(&[main]);
    let broken = builder.stack(&[work, cut]);
    let python = builder.stack(&[handle, work, main]);
    builder.sample(main_thread, offset + 5, 1000, deep, NONE);
    builder.sample(main_thread, offset + 2, 1000, shallow, NONE);
    builder.sample(worker, offset + 3, 1000, broken, NONE);
    builder.sample(worker, offset + 4, 1000, python, NONE);
    builder.off_cpu(worker, offset + 4, 2, false);
    builder.off_cpu(worker, offset + 1, 1, true);
    builder.count(AccountKind::LostSamples as u32, 2);
    builder.gap(offset + 6, offset + 7, GapReason::Budget);
    builder.marker(
        offset + 6,
        MarkerKind::Budget,
        "sampling half of each second",
    );
    let site = builder.site("handle", "app.rs", 9, module, 1);
    builder.zone(
        worker,
        Zone {
            start: (offset + 3).into(),
            end: (offset + 9).into(),
            site: site.into(),
            flags: 0.into(),
            value: NONE.into(),
            reserved: 0.into(),
        },
    );

    // A read that returned three bytes, behind a signal.
    let mut captured = vec![1, 1, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0];
    captured.extend_from_slice(b"abc");
    builder.syscall_note(
        SyscallNote {
            time: (offset + 7).into(),
            value: 10.into(),
            extra: 0.into(),
            thread: worker.into(),
            process: 0.into(),
            text: NONE.into(),
            stack: deep.into(),
            blob: NONE.into(),
            blob_len: 0.into(),
            calls: 0.into(),
            kind: NoteKind::Signal as u8,
            flags: 0,
            reserved: [0; 10],
        },
        &[0; 128],
    );
    builder.syscall(
        Syscall {
            entered: (offset + 7).into(),
            exited: (offset + 8).into(),
            ret: 3.into(),
            args: [3, 0x1000, 16, 0, 0, 0].map(Into::into),
            thread: worker.into(),
            number: 0.into(),
            stack: deep.into(),
            captures: NONE.into(),
            captures_len: 0.into(),
            abi: 0,
            outcome: CallOutcome::Returned as u8,
            flags: 0,
            reserved: 0,
        },
        &captured,
    );
    builder.finish()
}

#[test]
fn a_capture_seals_reads_back_and_files_merge_chunks_by_identity() {
    let capture = sample_capture(0);
    assert_eq!(capture.validate(), Ok(()));

    // Samples come out per thread, in time order.
    let first = capture.thread_samples(&capture.sample_ranges[0]);
    assert_eq!(
        first
            .iter()
            .map(|sample| sample.time.get())
            .collect::<Vec<_>>(),
        [2, 5]
    );
    let chunk = capture.seal().expect("seal");
    assert_eq!(
        Capture::from_chunk(chunk.as_bytes()).expect("read"),
        capture
    );

    // A file of two chunks of the same recording merges them: frames and
    // stacks are interned once, and each thread's samples stay ordered.
    let mut bytes = Vec::new();
    let mut writer = file::Writer::new(&mut bytes).expect("header");
    writer.chunk(&capture.seal().expect("seal")).expect("chunk");
    let later = {
        let mut capture = sample_capture(0);
        for sample in &mut capture.samples {
            sample.time = (sample.time.get() + 50).into();
        }

        for zone in &mut capture.zones {
            zone.start = (zone.start.get() + 50).into();
            zone.end = (zone.end.get() + 50).into();
        }

        for gap in &mut capture.gaps {
            gap.start = (gap.start.get() + 50).into();
            gap.end = (gap.end.get() + 50).into();
        }

        capture
    };

    writer.chunk(&later.seal().expect("seal")).expect("chunk");
    writer.finish().expect("index");
    let read = file::read(&bytes).expect("read the file");
    assert_eq!((read.chunks, read.truncated), (2, false));
    let merged = read.capture;
    assert_eq!(merged.validate(), Ok(()));
    assert_eq!(merged.frames.len(), capture.frames.len());
    assert_eq!(merged.stacks.len(), capture.stacks.len());
    assert_eq!(merged.threads.len(), 2);
    assert_eq!(merged.samples.len(), 8);
    assert_eq!(merged.zones.len(), 2);
    assert_eq!(merged.gaps.len(), 2);
    assert_eq!(merged.account(AccountKind::LostSamples as u32), 4);
    assert_eq!(merged.frame_lines(0).len(), 1);

    // A file cut inside its index, or inside its second chunk, still reads
    // every complete chunk, and says it was cut.
    let cut = &bytes[..bytes.len() - 3];
    let read = file::read(cut).expect("read a cut file");
    assert_eq!((read.chunks, read.truncated), (2, true));
    let first_chunk = 16 + 8 + capture.seal().expect("seal").as_bytes().len();
    let read = file::read(&bytes[..first_chunk + 100]).expect("read a file cut in a chunk");
    assert_eq!((read.chunks, read.truncated), (1, true));
}

/// An interpreted frame is keyed by its function, file, and line, and its
/// function by where its code begins, so that functions of one name in two
/// files, or at two places in one, stay apart, while one function's lines
/// share it. A capture rebuilt from others keys them by their text, not by
/// where each kept it.
#[test]
fn interpreted_frames_keep_their_functions_apart_through_a_merge() {
    let places = [
        ("handle", Some("a.py"), 12, Some(10)),
        ("handle", Some("a.py"), 14, Some(10)),
        ("handle", Some("b.py"), 12, Some(10)),
        ("handle", Some("a.py"), 31, Some(30)),
        ("handle", None, 0, None),
    ];
    let build = |padding: &str| {
        let mut builder = CaptureBuilder::new();

        // Moves every later string, so that the captures keep the same text
        // at different ids.
        builder.string(padding);
        let frames: Vec<u32> = places
            .iter()
            .map(|&(function, file, line, declared)| {
                builder.interpreted(&InterpretedPlace {
                    function,
                    file,
                    line,
                    declared,
                })
            })
            .collect();
        let again = builder.interpreted(&InterpretedPlace {
            function: "handle",
            file: Some("a.py"),
            line: 12,
            declared: Some(10),
        });
        assert_eq!(again, frames[0], "one place is one frame");
        let thread = builder.thread(Pid::new(1), Tid::new(1), 0);
        let stack = builder.stack(&frames);
        builder.sample(thread, 1, 1, stack, NONE);
        builder.finish()
    };

    let shown = |capture: &Capture| {
        assert_eq!(capture.validate(), Ok(()));
        capture
            .frames
            .iter()
            .enumerate()
            .map(|(frame, record)| {
                assert_eq!(record.kind, FrameKind::Interpreted as u8);
                assert_eq!(record.module.get(), NONE);
                let [line] = capture.frame_lines(u32::try_from(frame).expect("a frame")) else {
                    panic!("frame {frame} has one line");
                };
                let function = &capture.functions[line.function.get() as usize];
                (
                    line.function.get(),
                    capture.string(function.name.get()).to_owned(),
                    capture.string(line.file.get()).to_owned(),
                    line.number.get(),
                    capture.string(function.file.get()).to_owned(),
                    function.line.get(),
                )
            })
            .collect::<Vec<_>>()
    };

    let capture = build("");
    let rows = shown(&capture);
    assert_eq!(
        rows.iter()
            .map(|row| (row.1.as_str(), row.2.as_str(), row.3, row.4.as_str(), row.5))
            .collect::<Vec<_>>(),
        [
            ("handle", "a.py", 12, "a.py", 10),
            ("handle", "a.py", 14, "a.py", 10),
            ("handle", "b.py", 12, "b.py", 10),
            ("handle", "a.py", 31, "a.py", 30),
            ("handle", "", 0, "", 0),
        ]
    );
    let functions: Vec<u32> = rows.iter().map(|row| row.0).collect();
    assert_eq!(functions[0], functions[1], "one function's two lines");
    let distinct: std::collections::BTreeSet<u32> = functions.iter().copied().collect();
    assert_eq!(distinct.len(), 4, "four functions: {functions:?}");

    let mut merged = CaptureBuilder::new();
    merged.absorb(&capture);
    merged.absorb(&build("a string that moves the rest"));
    let merged = merged.finish();
    assert_eq!(shown(&merged), rows);
    assert_eq!(merged.samples.len(), 2);
    assert_eq!(merged.stacks.len(), capture.stacks.len());
}

/// Two zones of one span nest as they were recorded: the one recorded
/// later, which ended last, holds the other. A capture rebuilt from
/// another, as symbolizing or merging a file's chunks rebuilds it, keeps
/// that nesting.
#[test]
fn zones_of_one_span_keep_their_nesting_when_a_capture_is_rebuilt() {
    let mut builder = CaptureBuilder::new();
    let thread = builder.thread(Pid::new(1), Tid::new(1), 0);
    let site = |builder: &mut CaptureBuilder, name: &str| builder.site(name, "f.rs", 1, NONE, 0);
    let (inner, outer) = (site(&mut builder, "leaf"), site(&mut builder, "b"));
    for site in [inner, outer] {
        builder.zone(
            thread,
            Zone {
                start: 5.into(),
                end: 9.into(),
                site: site.into(),
                flags: 0.into(),
                value: NONE.into(),
                reserved: 0.into(),
            },
        );
    }

    let depths = |capture: &Capture| {
        capture
            .zone_ranges
            .iter()
            .flat_map(|range| {
                let start = range.start.get() as usize;
                capture.zones[start..start + range.count.get() as usize]
                    .iter()
                    .map(move |zone| (zone.site.get(), range.depth.get()))
            })
            .collect::<Vec<_>>()
    };

    let recorded = builder.finish();
    assert_eq!(depths(&recorded), [(outer, 0), (inner, 1)]);

    let mut rebuilt = CaptureBuilder::new();
    rebuilt.absorb(&recorded);
    assert_eq!(depths(&rebuilt.finish()), [(outer, 0), (inner, 1)]);
}

/// A task's labels, which snapshots find without a root frame, survive a
/// capture rebuilt from another, as symbolizing or merging a file's chunks
/// rebuilds it.
#[test]
fn merged_chunks_keep_each_tasks_labels() {
    let mut first = CaptureBuilder::new();
    let task = first.task("goroutine", 1, 0);
    first.describe_task(task, NONE, Some("phase=spin"));
    let first = first.finish();
    let mut second = CaptureBuilder::new();
    second.task("goroutine", 1, 10);
    let second = second.finish();

    let mut merged = CaptureBuilder::new();
    merged.absorb(&first);
    merged.absorb(&second);
    let merged = merged.finish();
    assert_eq!(merged.validate(), Ok(()));
    assert_eq!(merged.tasks.len(), 1);
    assert_ne!(merged.tasks[0].labels.get(), NONE, "the labels were lost");
    assert_eq!(merged.string(merged.tasks[0].labels.get()), "phase=spin");
}

/// Forgetting what came before a moment keeps every row from it on, cuts
/// the spans that cross it there, and keeps what rows refer to.
#[test]
fn a_builder_forgets_rows_before_a_moment() {
    let mut builder = CaptureBuilder::new();
    let thread = builder.thread(Pid::new(1), Tid::new(1), 0);
    let frame = builder.frame(FrameKind::Native, NONE, 0x10, 0);
    let stack = builder.stack(&[frame]);
    for time in [1, 5, 9] {
        builder.sample(thread, time, 1, stack, NONE);
    }

    builder.off_cpu(thread, 2, 2, false);
    builder.off_cpu(thread, 4, 4, true);
    builder.gap(1, 2, GapReason::Budget);
    builder.gap(3, 7, GapReason::Budget);
    builder.marker(3, MarkerKind::Budget, "before");
    builder.marker(6, MarkerKind::Budget, "after");
    builder.forget_before(5);
    let capture = builder.finish();
    assert_eq!(capture.validate(), Ok(()));
    let times = |rows: &[Sample]| rows.iter().map(|row| row.time.get()).collect::<Vec<_>>();
    assert_eq!(times(&capture.samples), [5, 9]);
    assert_eq!(
        capture
            .off_cpu
            .iter()
            .map(|row| (row.time.get(), row.duration.get(), row.preempted))
            .collect::<Vec<_>>(),
        [(5, 3, 1)]
    );
    assert_eq!(
        capture
            .gaps
            .iter()
            .map(|gap| (gap.start.get(), gap.end.get()))
            .collect::<Vec<_>>(),
        [(5, 7)]
    );
    assert_eq!(capture.markers.len(), 1);
    assert_eq!(capture.stacks.len(), 1);
}

#[test]
fn validation_rejects_every_out_of_bounds_reference() {
    let valid = sample_capture(0);
    let mut broken = valid.clone();
    broken.stacks[0].parent = 5.into();
    assert!(broken.validate().is_err(), "a parent after its child");
    let mut broken = valid.clone();
    broken.samples[0].stack = 99.into();
    assert!(broken.validate().is_err(), "a sample of no stack");
    let mut broken = valid.clone();
    broken.samples.swap(0, 1);
    assert!(broken.validate().is_err(), "samples out of order");
    let mut broken = valid.clone();
    broken.off_cpu.swap(0, 1);
    assert!(broken.validate().is_err(), "off-CPU intervals out of order");
    let mut broken = valid.clone();
    broken.off_cpu[0].preempted = 2;
    assert!(
        broken.validate().is_err(),
        "an off-CPU interval of no cause"
    );
    let mut broken = valid.clone();
    broken.gaps[0].end = (-1).into();
    assert!(broken.validate().is_err(), "a gap ending before it begins");
    let mut broken = valid.clone();
    broken.frames[0].module = 9.into();
    assert!(broken.validate().is_err(), "a frame of no module");
    let mut broken = valid.clone();
    broken.zones[0].end = (-1).into();
    assert!(broken.validate().is_err(), "a zone ending before it starts");
    let mut broken = valid.clone();
    broken.syscall_notes[0].calls = 2.into();
    assert!(broken.validate().is_err(), "a note after calls not there");
    let mut broken = valid.clone();
    broken.syscalls[0].captures_len = 14.into();
    assert!(broken.validate().is_err(), "captured bytes cut short");
    let mut broken = valid.clone();
    broken.syscall_notes[0].kind = 0;
    assert!(broken.validate().is_err(), "a note of no kind");
    let mut broken = valid.clone();
    let named = broken
        .frames
        .iter()
        .position(|frame| frame.kind == FrameKind::Interpreted as u8)
        .expect("an interpreted frame");
    broken.frames[named].address = u64::MAX.into();
    assert!(broken.validate().is_err(), "an interpreted frame of no key");
    let mut broken = valid.clone();
    broken.frames[named].kind = 6;
    assert!(broken.validate().is_err(), "a frame of no kind");
    let mut broken = valid;
    broken.strings.pop();
    assert!(broken.validate().is_err(), "an unterminated string pool");
}

/// The capture fuzzer's seed is a whole file of this revision, so that
/// the fuzzer starts past validation. `MICROSCOPE_UPDATE_SEED=1` rewrites it.
#[test]
fn the_fuzzers_seed_is_a_capture_of_this_revision() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus/capture/two-threads");
    let mut expected = Vec::new();
    file::write(&sample_capture(0), &mut expected).expect("write");
    if std::env::var_os("MICROSCOPE_UPDATE_SEED").is_some() {
        std::fs::write(&path, &expected).expect("write the seed");
    }

    let seed = std::fs::read(&path).expect("read the seed");
    assert!(
        seed == expected,
        "the seed is stale: rerun with MICROSCOPE_UPDATE_SEED=1"
    );
}
