//! Properties of merging and cutting captures, checked against a model of
//! what a recording saw: rebuilding a capture through `absorb` keeps it,
//! cutting it at any moments splits every row between the pieces, and a
//! file of the pieces reads back as them, however it is cut or damaged.

#![expect(
    clippy::too_many_lines,
    clippy::type_complexity,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "a model names every table's rows in one place, of small counts and times"
)]

use std::collections::BTreeMap;

use proptest::prelude::*;

use super::builder::{InterpretedPlace, LineInput};
use super::*;
use crate::model::{Pid, Tid};

/// The end of every recording the model makes.
const END: i64 = 100;

/// One life of a thread: its pid, tid, span, and name. A tid may have two
/// lives, in one process or two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Life {
    pid: u32,
    tid: u32,
    start: i64,
    end: i64,
    named: bool,
}

/// A system call or a note between calls, in the order they were traced.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Traced {
    Call {
        life: usize,
        entered: i64,
        exited: Option<i64>,
        captured: bool,
    },
    Note {
        life: Option<usize>,
        time: i64,
        text: Option<u8>,
        blob: bool,
    },
}

impl Traced {
    /// When a cut sees it happen.
    const fn time(&self) -> i64 {
        match self {
            Self::Call {
                entered, exited, ..
            } => match exited {
                Some(exited) => *exited,
                None => *entered,
            },
            Self::Note { time, .. } => *time,
        }
    }
}

/// What a recording saw, each row by the lives and choices it names.
#[derive(Debug, Clone)]
struct Model {
    lives: Vec<Life>,

    /// Stacks of [`frames`], innermost first.
    stacks: Vec<Vec<usize>>,

    /// Each of three tasks' labels, if any.
    labels: [Option<u8>; 3],
    samples: Vec<(usize, i64, u64, usize, Option<usize>)>,
    off_cpu: Vec<(usize, i64, u64, bool)>,

    /// Life, start, end, site, value, flags.
    zones: Vec<(usize, i64, i64, usize, Option<u8>, u32)>,

    /// A life's or the recording's, time, and text.
    marks: Vec<(Option<usize>, i64, Option<u8>)>,
    plots: Vec<(usize, usize, i64, i32)>,
    waits: Vec<(i64, u64, usize, usize)>,

    /// In time order, apart or touching.
    gaps: Vec<(i64, i64)>,

    /// In time order.
    traced: Vec<Traced>,
}

/// A read that returned three bytes, as the capture's blobs lay it out.
const CAPTURED: [u8; 15] = [1, 1, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, b'a', b'b', b'c'];

/// The kinds of frame stacks choose from.
const FRAMES: usize = 6;

/// Builds the model's frames: two native functions of one module, one
/// with a function inlined into it; one of another module with symbols
/// and one without; an interpreted frame; and a truncation.
fn frames(builder: &mut CaptureBuilder) -> [u32; FRAMES] {
    let app = builder.module("/bin/app", Some("b1"), [1; 16], 4096);
    let lib = builder.module("/lib/libx.so", None, [2; 16], 4096);
    let line = |function, line| LineInput {
        function,
        file: Some("app.c"),
        line,
        column: 0,
        declared: Some(("app.c", 1)),
        role: 0,
    };

    let main = builder.frame(FrameKind::Native, app, 0x10, 0);
    builder.symbolize(main, app, &[line("main", 3)]);
    let inlined = builder.frame(FrameKind::Native, app, 0x20, 0);
    builder.symbolize(inlined, app, &[line("inner", 5), line("outer", 9)]);
    let x = builder.frame(FrameKind::Native, lib, 0x30, 0);
    builder.symbolize(x, lib, &[line("x", 2)]);
    let bare = builder.frame(FrameKind::Native, lib, 0x40, 0);
    let handle = builder.interpreted(&InterpretedPlace {
        function: "handle",
        file: Some("a.py"),
        line: 3,
        declared: Some(("a.py", 1)),
    });
    let cut = builder.truncation(Truncation::FramePointerNotKept, app);

    [main, inlined, x, bare, handle, cut]
}

const SITES: [&str; 3] = ["load", "parse", "route"];

/// The capture of what the model saw, with settings and counts when
/// `counted`.
fn build(model: &Model, counted: bool) -> CaptureBuilder {
    let mut builder = CaptureBuilder::new();
    builder.setting("start", "0");
    builder.setting("end", &END.to_string());
    builder.setting("program", "/bin/app");
    let frames = frames(&mut builder);
    let stacks: Vec<u32> = model
        .stacks
        .iter()
        .map(|choices| {
            let stack: Vec<u32> = choices.iter().map(|&choice| frames[choice]).collect();
            builder.stack(&stack)
        })
        .collect();

    let tasks: Vec<u32> = (0..3)
        .map(|number| builder.task("goroutine", number + 1, 0))
        .collect();
    for (task, label) in tasks.iter().zip(model.labels) {
        if let Some(label) = label {
            builder.describe_task(*task, NONE, Some(&format!("phase=p{label}")));
        }
    }

    let sites: Vec<u32> = SITES
        .iter()
        .map(|name| builder.site(name, "app.rs", 1, NONE, 0))
        .collect();

    // Each tid's lives come one after another, each ended before the next
    // begins.
    let threads: Vec<u32> = model
        .lives
        .iter()
        .map(|life| {
            let thread = builder.thread(Pid::new(life.pid), Tid::new(life.tid), life.start);
            if life.named {
                builder.thread_name(thread, &format!("t{}", life.tid));
            }

            builder.thread_exit(Tid::new(life.tid), life.end);
            thread
        })
        .collect();

    let task = |choice: Option<usize>| choice.map_or(NONE, |choice| tasks[choice]);

    for &(life, time, weight, stack, choice) in &model.samples {
        builder.sample(threads[life], time, weight, stacks[stack], task(choice));
    }

    for &(life, time, duration, preempted) in &model.off_cpu {
        builder.off_cpu(threads[life], time, duration, preempted);
    }

    for &(life, start, end, site, value, flags) in &model.zones {
        let value = value.map_or(NONE, |value| builder.string(&format!("v{value}")));
        builder.zone(
            threads[life],
            Zone {
                start: start.into(),
                end: end.into(),
                site: sites[site].into(),
                flags: flags.into(),
                value: value.into(),
                reserved: 0.into(),
            },
        );
    }

    for &(life, time, text) in &model.marks {
        let text = text.map(|text| format!("m{text}"));
        match life {
            Some(life) => builder.thread_marker(
                (threads[life], sites[0]),
                time,
                MarkerKind::Message,
                text.as_deref(),
            ),
            None => builder.marker(time, MarkerKind::Budget, text.as_deref().unwrap_or("")),
        }
    }

    for &(life, site, time, value) in &model.plots {
        builder.plot(threads[life], sites[site], time, f64::from(value));
    }

    for &(time, weight, stack, choice) in &model.waits {
        builder.task_wait(tasks[choice], time, weight, stacks[stack], "chan receive");
    }

    for &(start, end) in &model.gaps {
        builder.gap(start, end, GapReason::Budget);
    }

    for traced in &model.traced {
        match *traced {
            Traced::Call {
                life,
                entered,
                exited,
                captured,
            } => builder.syscall(
                Syscall {
                    entered: entered.into(),
                    exited: exited.unwrap_or(UNSEEN).into(),
                    ret: 3.into(),
                    args: [3, 0x1000, 16, 0, 0, 0].map(Into::into),
                    thread: threads[life].into(),
                    number: 0.into(),
                    stack: stacks[0].into(),
                    captures: NONE.into(),
                    captures_len: 0.into(),
                    abi: 0,
                    outcome: if exited.is_some() {
                        CallOutcome::Returned
                    } else {
                        CallOutcome::Unfinished
                    } as u8,
                    flags: 0,
                    reserved: 0,
                },
                if captured { &CAPTURED[..] } else { &[] },
            ),
            Traced::Note {
                life,
                time,
                text,
                blob,
            } => {
                let text = text.map_or(NONE, |text| builder.string(&format!("n{text}")));
                let (thread, process) = life.map_or((NONE, NONE), |life| {
                    let pid = Pid::new(model.lives[life].pid);
                    (threads[life], builder.process(pid, time))
                });
                builder.syscall_note(
                    SyscallNote {
                        time: time.into(),
                        value: 10.into(),
                        extra: 0.into(),
                        thread: thread.into(),
                        process: process.into(),
                        text: text.into(),
                        stack: stacks[0].into(),
                        blob: NONE.into(),
                        blob_len: 0.into(),
                        calls: 0.into(),
                        kind: NoteKind::Signal as u8,
                        flags: 0,
                        reserved: [0; 10],
                    },
                    if blob { &[7; 9] } else { &[] },
                );
            }
        }
    }

    if counted {
        builder.count(AccountKind::LostSamples as u32, 2);
        builder.count(AccountKind::LostRecords as u32, 5);
    }

    builder
}

/// A thread as rows show it: pid, tid, span, and name.
type ThreadKey = (u32, u32, i64, i64, String);

/// A capture's rows by what they say, not where they are: every id is
/// replaced by what it names, and every table is in a fixed order.
#[derive(Debug, Clone, Default, PartialEq)]
struct Rows {
    settings: BTreeMap<String, String>,
    threads: Vec<ThreadKey>,
    tasks: Vec<(String, u64, String)>,
    samples: Vec<(ThreadKey, i64, u64, Vec<String>, Option<u64>)>,
    off_cpu: Vec<(ThreadKey, i64, u64, u8)>,

    /// Thread, start, end, site, flags, value, and depth when it is kept.
    zones: Vec<(ThreadKey, i64, i64, String, u32, String, Option<u32>)>,
    markers: Vec<(Option<ThreadKey>, i64, u8, String)>,
    plots: Vec<(ThreadKey, String, i64, u64)>,
    waits: Vec<(i64, u64, u64, Vec<String>, String)>,
    gaps: Vec<(i64, i64)>,
    traced: Vec<TracedRow>,
    accounts: BTreeMap<u32, u64>,
}

#[derive(Debug, Clone, PartialEq)]
enum TracedRow {
    Call(ThreadKey, i64, i64, Vec<u8>),
    Note(Option<ThreadKey>, Option<u32>, i64, String, Vec<u8>),
}

fn thread_key(capture: &Capture, thread: u32) -> ThreadKey {
    let row = &capture.threads[thread as usize];
    (
        capture.processes[row.process.get() as usize].pid.get(),
        row.tid.get(),
        row.start.get(),
        row.end.get(),
        capture.string(row.name.get()).to_owned(),
    )
}

fn frame_text(capture: &Capture, frame: u32) -> String {
    let row = &capture.frames[frame as usize];
    let module = match row.module.get() {
        NONE => "",
        module => capture.string(capture.modules[module as usize].path.get()),
    };
    let named = FrameKind::from_number(row.kind).is_some_and(FrameKind::named);
    let address = if named {
        capture
            .string(u32::try_from(row.address.get()).unwrap_or(NONE))
            .to_owned()
    } else {
        format!("{:#x}", row.address.get())
    };
    let lines: Vec<String> = capture
        .frame_lines(frame)
        .iter()
        .map(|line| {
            let function = &capture.functions[line.function.get() as usize];
            format!(
                "{} {}:{} [{}:{}]",
                capture.string(function.name.get()),
                capture.string(line.file.get()),
                line.number.get(),
                capture.string(function.file.get()),
                function.line.get(),
            )
        })
        .collect();

    format!("{} {module} {address} {lines:?}", row.kind)
}

fn stack_text(capture: &Capture, stack: u32) -> Vec<String> {
    capture
        .stack_frames(stack)
        .map(|frame| frame_text(capture, frame))
        .collect()
}

fn blob(bytes: &[u8], start: u32, len: u32) -> Vec<u8> {
    if start == NONE {
        Vec::new()
    } else {
        bytes[start as usize..(start + len) as usize].to_vec()
    }
}

/// What `capture` says, with zones' depths when `depths`.
fn rows(capture: &Capture, depths: bool) -> Rows {
    let thread = |thread: u32| thread_key(capture, thread);
    let task = |task: u32| (task != NONE).then(|| capture.tasks[task as usize].number.get());
    let mut rows = Rows {
        settings: capture
            .settings
            .iter()
            .map(|setting| {
                (
                    capture.string(setting.key.get()).to_owned(),
                    capture.string(setting.value.get()).to_owned(),
                )
            })
            .collect(),
        accounts: capture
            .accounting
            .iter()
            .map(|account| (account.kind.get(), account.value.get()))
            .collect(),
        ..Rows::default()
    };

    rows.threads = (0..capture.threads.len() as u32).map(thread).collect();
    rows.tasks = capture
        .tasks
        .iter()
        .map(|task| {
            (
                capture.string(task.runtime.get()).to_owned(),
                task.number.get(),
                capture.string(task.labels.get()).to_owned(),
            )
        })
        .collect();

    for range in &capture.sample_ranges {
        for sample in capture.thread_samples(range) {
            rows.samples.push((
                thread(range.thread.get()),
                sample.time.get(),
                sample.weight.get(),
                stack_text(capture, sample.stack.get()),
                task(sample.task.get()),
            ));
        }
    }

    for range in &capture.off_cpu_ranges {
        for interval in capture.thread_off_cpu(range) {
            rows.off_cpu.push((
                thread(range.thread.get()),
                interval.time.get(),
                interval.duration.get(),
                interval.preempted,
            ));
        }
    }

    for range in &capture.zone_ranges {
        let start = range.start.get() as usize;
        for zone in &capture.zones[start..start + range.count.get() as usize] {
            rows.zones.push((
                thread(range.thread.get()),
                zone.start.get(),
                zone.end.get(),
                capture
                    .string(capture.sites[zone.site.get() as usize].name.get())
                    .to_owned(),
                zone.flags.get(),
                capture.string(zone.value.get()).to_owned(),
                depths.then(|| range.depth.get()),
            ));
        }
    }

    rows.markers = capture
        .markers
        .iter()
        .map(|marker| {
            (
                (marker.thread.get() != NONE).then(|| thread(marker.thread.get())),
                marker.time.get(),
                marker.kind,
                capture.string(marker.text.get()).to_owned(),
            )
        })
        .collect();
    rows.plots = capture
        .plots
        .iter()
        .map(|plot| {
            (
                thread(plot.thread.get()),
                capture
                    .string(capture.sites[plot.site.get() as usize].name.get())
                    .to_owned(),
                plot.time.get(),
                plot.value.get(),
            )
        })
        .collect();
    rows.waits = capture
        .task_waits
        .iter()
        .map(|wait| {
            (
                wait.time.get(),
                wait.weight.get(),
                capture.tasks[wait.task.get() as usize].number.get(),
                stack_text(capture, wait.stack.get()),
                capture.string(wait.state.get()).to_owned(),
            )
        })
        .collect();
    rows.gaps = capture
        .gaps
        .iter()
        .map(|gap| (gap.start.get(), gap.end.get()))
        .collect();

    // Calls and the notes between them, in the order they were traced.
    let mut notes = capture.syscall_notes.iter().peekable();
    for (position, call) in capture.syscalls.iter().enumerate() {
        while let Some(note) = notes.next_if(|note| note.calls.get() as usize <= position) {
            rows.traced.push(note_row(capture, note));
        }

        rows.traced.push(TracedRow::Call(
            thread(call.thread.get()),
            call.entered.get(),
            call.exited.get(),
            blob(
                &capture.syscall_blobs,
                call.captures.get(),
                call.captures_len.get(),
            ),
        ));
    }

    rows.traced
        .extend(notes.map(|note| note_row(capture, note)));

    rows.sort();
    rows
}

fn note_row(capture: &Capture, note: &SyscallNote) -> TracedRow {
    TracedRow::Note(
        (note.thread.get() != NONE).then(|| thread_key(capture, note.thread.get())),
        (note.process.get() != NONE)
            .then(|| capture.processes[note.process.get() as usize].pid.get()),
        note.time.get(),
        capture.string(note.text.get()).to_owned(),
        blob(&capture.syscall_blobs, note.blob.get(), note.blob_len.get()),
    )
}

impl Rows {
    /// Every table in one order, but the traced calls, whose order is
    /// what they say.
    fn sort(&mut self) {
        self.threads.sort();
        self.tasks.sort();
        self.samples.sort();
        self.off_cpu.sort();
        self.zones.sort();
        self.markers.sort();
        self.plots.sort();
        self.waits.sort();
        self.gaps.sort_unstable();
    }
}

/// The rows the model's recording holds of the moments `lo..hi`: each
/// row of a moment in it, and each span that overlaps it, cut to it and
/// flagged where a zone was cut. A span of no length is a moment.
fn expected(model: &Model, (lo, hi): (i64, i64)) -> Rows {
    let frames = expected_frames();
    let stack = |stack: usize| -> Vec<String> {
        model.stacks[stack]
            .iter()
            .map(|&choice| frames[choice].clone())
            .collect()
    };
    let thread = |life: usize| {
        let life = model.lives[life];
        let name = if life.named {
            format!("t{}", life.tid)
        } else {
            String::new()
        };
        (life.pid, life.tid, life.start, life.end, name)
    };
    let at = |time: i64| lo <= time && time < hi;
    let overlaps = |start: i64, end: i64| {
        if start == end {
            at(start)
        } else {
            start < hi && end > lo
        }
    };

    let mut settings = BTreeMap::from([
        ("start".to_owned(), "0".to_owned()),
        ("end".to_owned(), END.to_string()),
        ("program".to_owned(), "/bin/app".to_owned()),
    ]);
    if lo > i64::MIN {
        settings.insert("start".to_owned(), lo.to_string());
    }
    if hi < i64::MAX {
        settings.insert("end".to_owned(), hi.to_string());
    }

    let mut rows = Rows {
        settings,
        threads: (0..model.lives.len()).map(thread).collect(),
        tasks: model
            .labels
            .iter()
            .zip(1..)
            .map(|(label, number)| {
                let label = label.map_or_else(String::new, |label| format!("phase=p{label}"));
                ("goroutine".to_owned(), number, label)
            })
            .collect(),
        ..Rows::default()
    };

    for &(life, time, weight, choice, task) in &model.samples {
        if at(time) {
            let task = task.map(|task| task as u64 + 1);
            rows.samples
                .push((thread(life), time, weight, stack(choice), task));
        }
    }

    for &(life, start, duration, preempted) in &model.off_cpu {
        let end = start + duration as i64;
        if overlaps(start, end) {
            let (start, end) = (start.max(lo), end.min(hi));
            rows.off_cpu.push((
                thread(life),
                start,
                (end - start) as u64,
                u8::from(preempted),
            ));
        }
    }

    for &(life, start, end, site, value, flags) in &model.zones {
        if overlaps(start, end) {
            let flags = flags
                | if start < lo { ZONE_BEGAN_UNSEEN } else { 0 }
                | if end > hi { ZONE_ENDED_UNSEEN } else { 0 };
            let value = value.map_or_else(String::new, |value| format!("v{value}"));
            rows.zones.push((
                thread(life),
                start.max(lo),
                end.min(hi),
                SITES[site].to_owned(),
                flags,
                value,
                None,
            ));
        }
    }

    for &(life, time, text) in &model.marks {
        if at(time) {
            let kind = life.map_or(MarkerKind::Budget, |_| MarkerKind::Message);
            let thread = life.map(thread);
            let text = text.map_or_else(String::new, |text| format!("m{text}"));
            rows.markers.push((thread, time, kind as u8, text));
        }
    }

    for &(life, site, time, value) in &model.plots {
        if at(time) {
            rows.plots.push((
                thread(life),
                SITES[site].to_owned(),
                time,
                f64::from(value).to_bits(),
            ));
        }
    }

    for &(time, weight, choice, task) in &model.waits {
        if at(time) {
            rows.waits.push((
                time,
                weight,
                task as u64 + 1,
                stack(choice),
                "chan receive".to_owned(),
            ));
        }
    }

    for &(start, end) in &model.gaps {
        if overlaps(start, end) {
            rows.gaps.push((start.max(lo), end.min(hi)));
        }
    }

    for traced in model.traced.iter().filter(|traced| at(traced.time())) {
        rows.traced.push(match *traced {
            Traced::Call {
                life,
                entered,
                exited,
                captured,
            } => TracedRow::Call(
                thread(life),
                entered,
                exited.unwrap_or(UNSEEN),
                if captured {
                    CAPTURED.to_vec()
                } else {
                    Vec::new()
                },
            ),
            Traced::Note {
                life,
                time,
                text,
                blob,
            } => TracedRow::Note(
                life.map(thread),
                life.map(|life| model.lives[life].pid),
                time,
                text.map_or_else(String::new, |text| format!("n{text}")),
                if blob { vec![7; 9] } else { Vec::new() },
            ),
        });
    }

    rows.sort();
    rows
}

/// The frames [`frames`] makes, as [`frame_text`] shows them.
fn expected_frames() -> Vec<String> {
    let mut builder = CaptureBuilder::new();
    let frames = frames(&mut builder);
    let capture = builder.finish();
    frames
        .iter()
        .map(|&frame| frame_text(&capture, frame))
        .collect()
}

/// The rows of several pieces as one: the moments of each piece, every
/// span cut at the pieces' bounds.
fn expected_pieces(model: &Model, cuts: &[i64]) -> Rows {
    let bounds: Vec<i64> = std::iter::once(i64::MIN)
        .chain(cuts.iter().copied())
        .chain(std::iter::once(i64::MAX))
        .collect();
    let mut all = Rows::default();

    for (index, window) in bounds.windows(2).enumerate() {
        let piece = expected(model, (window[0], window[1]));
        if index == 0 {
            all.settings = piece.settings.clone();
            all.threads = piece.threads.clone();
            all.tasks = piece.tasks.clone();
        }

        all.samples.extend(piece.samples);
        all.off_cpu.extend(piece.off_cpu);
        all.zones.extend(piece.zones);
        all.markers.extend(piece.markers);
        all.plots.extend(piece.plots);
        all.waits.extend(piece.waits);
        all.gaps.extend(piece.gaps);
        all.traced.extend(piece.traced);
    }

    // Merged pieces span the recording and every cut.
    let first = cuts.first().map_or(0, |&cut| cut.min(0));
    let last = cuts.last().map_or(END, |&cut| cut.max(END));
    all.settings.insert("start".to_owned(), first.to_string());
    all.settings.insert("end".to_owned(), last.to_string());
    all.sort();
    all
}

/// Cuts a recording at `cuts`, in order, as a continuous recording's
/// segments do: each piece from the last cut to the next.
fn pieces(mut builder: CaptureBuilder, cuts: &[i64]) -> Vec<Capture> {
    let mut pieces = Vec::new();

    for &cut in cuts {
        let mut piece = builder.clone();
        piece.forget_from(cut);
        piece.setting("end", &cut.to_string());
        pieces.push(piece.finish());
        builder.forget_before(cut);
        builder.setting("start", &cut.to_string());
    }

    pieces.push(builder.finish());
    pieces
}

fn merged(captures: &[Capture]) -> Capture {
    let mut builder = CaptureBuilder::new();
    for capture in captures {
        builder.absorb(capture);
    }

    builder.finish()
}

/// Lives of three tids, the second life of a tid beginning after the
/// first ends, in the same process or another.
fn lives() -> impl Strategy<Value = Vec<Life>> {
    let tid = |tid| {
        (
            (1..=2_u32, 0..END, any::<bool>()),
            prop::option::of((1..=2_u32, 1..20_i64, 0..20_i64)),
            any::<bool>(),
        )
            .prop_map(move |((pid, length, named), again, both_named)| {
                let first = Life {
                    pid,
                    tid,
                    start: 0,
                    end: length,
                    named,
                };
                let mut lives = vec![first];
                if let Some((pid, after, length)) = again {
                    let start = first.end + after;
                    lives.push(Life {
                        pid,
                        tid,
                        start,
                        end: start + length,
                        named: both_named,
                    });
                }

                lives
            })
    };

    [tid(1), tid(2), tid(3)].prop_map(|lives| lives.concat())
}

/// A moment in `life`, from any draw.
fn within(life: &Life, draw: i64) -> i64 {
    life.start + draw.rem_euclid(life.end - life.start + 1)
}

fn model() -> impl Strategy<Value = Model> {
    let stacks = prop::collection::vec(prop::collection::vec(0..FRAMES, 1..4), 1..4);
    (lives(), stacks).prop_flat_map(|(lives, stacks)| {
        let life = 0..lives.len();
        let stack = 0..stacks.len();
        let time = 0..=END;
        let samples = prop::collection::vec(
            (
                life.clone(),
                time.clone(),
                1..5_u64,
                stack.clone(),
                prop::option::of(0..3_usize),
            ),
            0..12,
        );
        let off_cpu = prop::collection::vec(
            (
                life.clone(),
                time.clone(),
                prop_oneof![Just(0_u64), 0..30_u64],
                any::<bool>(),
            ),
            0..6,
        );
        let zones = prop::collection::vec(
            (
                life.clone(),
                time.clone(),
                prop_oneof![Just(0_i64), 0..40_i64],
                0..SITES.len(),
                prop::option::of(0..2_u8),
                prop::sample::select(vec![0, ZONE_MISMATCHED, ZONE_LOST_WITHIN]),
            ),
            0..8,
        );
        let marks = prop::collection::vec(
            (
                prop::option::of(life.clone()),
                time.clone(),
                prop::option::of(0..3_u8),
            ),
            0..4,
        );
        let plots = prop::collection::vec(
            (life.clone(), 0..SITES.len(), time.clone(), -3..3_i32),
            0..4,
        );
        let waits = prop::collection::vec((time.clone(), 1..5_u64, stack, 0..3_usize), 0..4);
        let gaps = prop::collection::vec((0..15_i64, prop_oneof![Just(0_i64), 0..15_i64]), 0..4);
        let traced = prop::collection::vec(
            (
                any::<bool>(),
                life,
                time,
                prop::option::of(0..5_i64),
                prop::option::of(0..2_u8),
                any::<bool>(),
            ),
            0..8,
        );
        let labels = prop::array::uniform3(prop::option::of(0..2_u8));

        (
            Just((lives, stacks)),
            labels,
            (samples, off_cpu, zones),
            (marks, plots, waits),
            (gaps, traced),
        )
            .prop_map(
                |(
                    (lives, stacks),
                    labels,
                    (samples, off_cpu, zones),
                    (marks, plots, waits),
                    (gaps, traced),
                )| {
                    let at = |life: usize, time: i64| within(&lives[life], time);
                    let mut end = 0;
                    let gaps = gaps
                        .into_iter()
                        .map(|(after, length)| {
                            let start = end + after;
                            end = start + length;
                            (start, end)
                        })
                        .collect();
                    let mut traced: Vec<Traced> = traced
                        .into_iter()
                        .map(|(call, life, time, length, text, flag)| {
                            let time = at(life, time);
                            if call {
                                let exited = length.map(|_| time);
                                let entered = time - length.unwrap_or(0);
                                Traced::Call {
                                    life,
                                    entered,
                                    exited,
                                    captured: flag,
                                }
                            } else {
                                Traced::Note {
                                    life: length.map(|_| life),
                                    time,
                                    text,
                                    blob: flag,
                                }
                            }
                        })
                        .collect();
                    traced.sort_by_key(Traced::time);

                    Model {
                        samples: samples
                            .into_iter()
                            .map(|(life, time, weight, stack, task)| {
                                (life, at(life, time), weight, stack, task)
                            })
                            .collect(),
                        off_cpu: off_cpu
                            .into_iter()
                            .map(|(life, time, duration, preempted)| {
                                (life, at(life, time), duration, preempted)
                            })
                            .collect(),
                        zones: zones
                            .into_iter()
                            .map(|(life, start, length, site, value, flags)| {
                                let start = at(life, start);
                                (life, start, start + length, site, value, flags)
                            })
                            .collect(),
                        marks: marks
                            .into_iter()
                            .map(|(life, time, text)| {
                                (life, life.map_or(time, |life| at(life, time)), text)
                            })
                            .collect(),
                        plots: plots
                            .into_iter()
                            .map(|(life, site, time, value)| (life, site, at(life, time), value))
                            .collect(),
                        lives,
                        stacks,
                        labels,
                        waits,
                        gaps,
                        traced,
                    }
                },
            )
    })
}

/// Moments to cut at: often a moment some row has, so that spans of no
/// length and spans that end there meet the cut.
fn cuts(model: &Model, draws: &[(bool, prop::sample::Index, i64)]) -> Vec<i64> {
    let mut moments: Vec<i64> = model
        .samples
        .iter()
        .map(|sample| sample.1)
        .chain(
            model
                .off_cpu
                .iter()
                .flat_map(|interval| [interval.1, interval.1 + interval.2.cast_signed()]),
        )
        .chain(model.zones.iter().flat_map(|zone| [zone.1, zone.2]))
        .chain(model.gaps.iter().flat_map(|&gap| <[i64; 2]>::from(gap)))
        .chain(model.traced.iter().map(Traced::time))
        .collect();
    moments.push(0);

    let mut cuts: Vec<i64> = draws
        .iter()
        .map(
            |&(from_rows, index, any)| {
                if from_rows { *index.get(&moments) } else { any }
            },
        )
        .collect();
    cuts.sort_unstable();
    cuts.dedup();
    cuts
}

fn cut_draws() -> impl Strategy<Value = Vec<(bool, prop::sample::Index, i64)>> {
    prop::collection::vec(
        (any::<bool>(), any::<prop::sample::Index>(), -1..=END + 1),
        1..4,
    )
}

/// A file of `captures`, a chunk each, and where each chunk ends.
fn file_of(captures: &[Capture]) -> (Vec<u8>, Vec<usize>) {
    let mut bytes = Vec::new();
    let mut ends = Vec::new();
    let mut writer = file::Writer::new(&mut bytes).expect("a header");
    let mut written = 16;

    for capture in captures {
        let chunk = capture.seal().expect("a sealed chunk");
        writer.chunk(&chunk).expect("a chunk");
        written += 8 + chunk.as_bytes().len();
        ends.push(written);
    }

    writer.finish().expect("an index");
    (bytes, ends)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// A capture rebuilt through `absorb` says what it said: every row,
    /// zone depth, task label, gap, and note among its calls, a reused
    /// tid still two threads. And what it says is what was recorded.
    #[test]
    fn a_rebuilt_capture_keeps_every_row(model in model()) {
        let original = build(&model, true).finish();
        prop_assert_eq!(original.validate(), Ok(()));

        let mut recorded = expected(&model, (i64::MIN, i64::MAX));
        recorded.accounts = BTreeMap::from([
            (AccountKind::LostSamples as u32, 2),
            (AccountKind::LostRecords as u32, 5),
        ]);
        prop_assert_eq!(&rows(&original, false), &recorded);

        let rebuilt = merged(std::slice::from_ref(&original));
        prop_assert_eq!(rebuilt.validate(), Ok(()));
        prop_assert_eq!(rows(&rebuilt, true), rows(&original, true));

        // Rebuilding again changes nothing, not even where rows are.
        prop_assert_eq!(merged(std::slice::from_ref(&rebuilt)), rebuilt);
    }

    /// Cut at any moments, a recording's pieces hold each row of it in
    /// exactly the piece of its moment, and each span in every piece it
    /// overlaps, cut there and flagged; merged, they are the recording
    /// with its spans cut.
    #[test]
    fn cut_pieces_split_every_row_and_merge_back(model in model(), draws in cut_draws()) {
        let cuts = cuts(&model, &draws);
        let pieces = pieces(build(&model, false), &cuts);
        let bounds: Vec<i64> = std::iter::once(i64::MIN)
            .chain(cuts.iter().copied())
            .chain(std::iter::once(i64::MAX))
            .collect();

        for (piece, window) in pieces.iter().zip(bounds.windows(2)) {
            prop_assert_eq!(piece.validate(), Ok(()));
            prop_assert_eq!(rows(piece, false), expected(&model, (window[0], window[1])), "{:?}", window);
        }

        let whole = merged(&pieces);
        prop_assert_eq!(whole.validate(), Ok(()));
        prop_assert_eq!(rows(&whole, false), expected_pieces(&model, &cuts));
    }

    /// A file of a recording's pieces reads back as them merged; cut at
    /// any byte, it reads exactly the chunks that end by the cut, and says
    /// it was cut; with any byte changed, it reads as it was or not at
    /// all.
    #[test]
    fn a_file_reads_its_whole_chunks_and_nothing_else(
        model in model(),
        draws in cut_draws(),
        at in prop::collection::vec(any::<prop::sample::Index>(), 4),
        flips in prop::collection::vec((any::<prop::sample::Index>(), 1..=255_u8), 6),
    ) {
        let pieces = pieces(build(&model, false), &cuts(&model, &draws));
        let (bytes, ends) = file_of(&pieces);
        let read = file::read(&bytes).expect("the file");
        prop_assert_eq!((read.chunks, read.truncated), (pieces.len(), false));
        prop_assert_eq!(read.capture.validate(), Ok(()));
        let whole = if pieces.len() == 1 { pieces[0].clone() } else { merged(&pieces) };
        prop_assert_eq!(&read.capture, &whole);

        // Every chunk's bounds, the index's, and some bytes between.
        let mut cuts: Vec<usize> = ends
            .iter()
            .flat_map(|&end| [end - 1, end, end + 1])
            .chain([0, 15, 16, 17, 24, bytes.len() - 1, bytes.len() - 16])
            .chain(at.iter().map(|index| index.index(bytes.len() + 1)))
            .collect();
        cuts.sort_unstable();
        cuts.dedup();

        for cut in cuts.into_iter().filter(|&cut| cut <= bytes.len()) {
            let whole_chunks = ends.iter().filter(|&&end| end <= cut).count();
            match file::read(&bytes[..cut]) {
                Ok(read) => {
                    prop_assert_eq!((read.chunks, read.truncated), (whole_chunks, cut < bytes.len()), "cut at {}", cut);
                    let expected = if whole_chunks == 1 { pieces[0].clone() } else { merged(&pieces[..whole_chunks]) };
                    prop_assert_eq!(read.capture, expected, "cut at {}", cut);
                }
                Err(error) => {
                    prop_assert_eq!(whole_chunks, 0, "cut at {}: {}", cut, error);
                    let refused = if cut < 16 { file::FileError::NotACapture } else { file::FileError::Empty };
                    prop_assert_eq!(error, refused, "cut at {}", cut);
                }
            }
        }

        for (index, flip) in flips {
            let mut damaged = bytes.clone();
            let position = index.index(bytes.len());
            damaged[position] ^= flip;
            if let Ok(read) = file::read(&damaged) {
                prop_assert_eq!(read.chunks, pieces.len(), "byte {} changed", position);
                prop_assert!(read.capture == read_whole(&bytes), "byte {} changed", position);
            }
        }
    }
}

fn read_whole(bytes: &[u8]) -> Capture {
    file::read(bytes).expect("the file").capture
}

/// Found by the cutting property: a thread off its CPU for no time, and a
/// gap of no length, at the moment of a cut were in neither piece.
#[test]
fn a_span_of_no_length_at_a_cut_is_in_the_later_piece() {
    let mut builder = CaptureBuilder::new();
    let thread = builder.thread(Pid::new(1), Tid::new(1), 0);
    builder.off_cpu(thread, 24, 0, true);
    builder.gap(24, 24, GapReason::Budget);
    let [before, after] = &pieces(builder, &[24])[..] else {
        panic!("two pieces");
    };

    assert!(before.off_cpu.is_empty() && before.gaps.is_empty());
    let interval = after
        .off_cpu
        .first()
        .map(|row| (row.time.get(), row.duration.get()));
    assert_eq!(interval, Some((24, 0)));
    let gap = after
        .gaps
        .first()
        .map(|gap| (gap.start.get(), gap.end.get()));
    assert_eq!(gap, Some((24, 24)));
}

/// Found by the rebuilding property: two zones of no length at one moment
/// swapped places each time a capture was rebuilt.
#[test]
fn zones_of_one_moment_keep_their_order_when_rebuilt() {
    let mut builder = CaptureBuilder::new();
    let thread = builder.thread(Pid::new(1), Tid::new(1), 0);
    let site = builder.site("load", "app.rs", 1, NONE, 0);

    for flags in [ZONE_MISMATCHED, 0] {
        builder.zone(
            thread,
            Zone {
                start: 2.into(),
                end: 2.into(),
                site: site.into(),
                flags: flags.into(),
                value: NONE.into(),
                reserved: 0.into(),
            },
        );
    }

    let recorded = builder.finish();
    assert_eq!(merged(std::slice::from_ref(&recorded)), recorded);
}
