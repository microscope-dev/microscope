use std::collections::BTreeMap;

use proptest::prelude::*;

use super::*;
use crate::capture::Capture;
use crate::capture::builder::{CaptureBuilder, InterpretedPlace, LineInput};
use crate::capture::records::{FrameKind, NONE, Truncation};
use crate::model::{Pid, Tid};

const FUNCTIONS: [&str; 5] = ["a", "b", "c", "d", "e"];

const UNNAMED: u32 = 3;

fn task_row(choice: u32) -> u32 {
    if choice == UNNAMED { NONE } else { choice }
}

fn group_name(choice: u32) -> String {
    match choice {
        UNNAMED => "[no task known]".to_owned(),
        1 => "goroutine 2 {phase=spin}".to_owned(),
        _ => format!("goroutine {}", choice + 1),
    }
}

fn capture(
    stacks: &[(Vec<usize>, bool)],
    samples: &[(u32, i64, u64, usize, u32)],
    waits: &[(i64, u64, usize, u32)],
) -> Capture {
    let mut builder = CaptureBuilder::new();
    let module = builder.module("/bin/app", None, [0; 16], 0);
    let at = |function, number| LineInput {
        line: number,
        ..line(function)
    };

    let mut frames = Vec::new();
    for (index, name) in (0..).zip(FUNCTIONS) {
        let frame = builder.frame(FrameKind::Native, module, 0x10 * u64::from(index), 0);
        builder.symbolize(frame, module, &[at(name, index + 1)]);
        frames.push(frame);
    }

    frames.push(builder.frame(FrameKind::Native, module, 0x500, 0));
    for (address, lines) in [
        (0x600, [at("b", 7), at("a", 8)]),
        (0x700, [at("a", 1), at("c", 9)]),
    ] {
        let frame = builder.frame(FrameKind::Native, module, address, 0);
        builder.symbolize(frame, module, &lines);
        frames.push(frame);
    }

    let remade = builder.module("/bin/app", None, [1; 16], 0);
    let other = builder.module("/lib/libz.so", None, [2; 16], 0);
    for (module, address) in [(remade, 0x800), (other, 0x10)] {
        let frame = builder.frame(FrameKind::Native, module, address, 0);
        builder.symbolize(frame, module, &[at("a", 1)]);
        frames.push(frame);
    }

    let truncation = builder.truncation(Truncation::FramePointerNotKept, NONE);
    let stacks: Vec<u32> = stacks
        .iter()
        .map(|(choices, truncated)| {
            let mut stack: Vec<u32> = choices.iter().map(|choice| frames[*choice]).collect();
            if *truncated {
                stack.push(truncation);
            }

            builder.stack(&stack)
        })
        .collect();

    let threads = [
        builder.thread(Pid::new(1), Tid::new(1), 0),
        builder.thread(Pid::new(1), Tid::new(2), 0),
        builder.thread(Pid::new(1), Tid::new(3), 0),
    ];

    let tasks: Vec<u32> = (1..=3)
        .map(|number| builder.task("goroutine", number, 0))
        .collect();
    builder.describe_task(tasks[1], NONE, Some("phase=spin"));

    for &(thread, time, weight, stack, task) in samples {
        builder.sample(
            threads[thread as usize],
            time,
            weight,
            stacks[stack],
            task_row(task),
        );
    }

    for &(time, weight, stack, task) in waits {
        builder.task_wait(task_row(task), time, weight, stacks[stack], "chan receive");
    }

    builder.finish()
}

fn line(function: &str) -> LineInput<'_> {
    LineInput {
        function,
        file: Some("app.c"),
        line: 1,
        column: 0,
        declared: None,
        role: 0,
    }
}

#[derive(Debug, Clone)]
enum Named {
    Focus(String),
    Merge(String),
    Drop(String),
    Collapse(String),
    Cycles,
}

fn loop_erased(path: &[String]) -> Vec<String> {
    let mut kept: Vec<String> = Vec::new();
    for step in path {
        match kept.iter().position(|earlier| earlier == step) {
            Some(at) => kept.truncate(at + 1),
            None => kept.push(step.clone()),
        }
    }

    kept
}

fn shown(keys: &Keys<'_>, key: u32) -> String {
    format!(
        "{} [{}]",
        keys.name(key),
        keys.module(key).unwrap_or_default()
    )
}

fn naive_frame(capture: &Capture, frame: u32, granularity: Granularity) -> Vec<String> {
    let record = &capture.frames[frame as usize];
    if record.kind == FrameKind::Truncated as u8 {
        return vec!["[frame pointer not kept] []".to_owned()];
    }

    let module = |module: u32| {
        let path = capture.string(capture.modules[module as usize].path.get());
        path.rsplit('/').next().unwrap_or(path)
    };

    let lines = capture.frame_lines(frame);
    let lines = match granularity {
        Granularity::Address => &[][..],
        Granularity::PhysicalFunction => &lines[lines.len().saturating_sub(1)..],
        Granularity::Function | Granularity::Line => lines,
    };

    if lines.is_empty() {
        let module = module(record.module.get());
        return vec![format!("{module}+{:#x} [{module}]", record.address.get())];
    }

    lines
        .iter()
        .map(|line| {
            let function = &capture.functions[line.function.get() as usize];
            let name = capture.string(function.name.get());
            let module = module(function.module.get());
            if granularity == Granularity::Line {
                let file = capture.string(line.file.get());
                format!("{name} {file}:{} [{module}]", line.number.get())
            } else {
                format!("{name} [{module}]")
            }
        })
        .collect()
}

fn naive_path(
    capture: &Capture,
    (stack, granularity): (u32, Granularity),
    invert: bool,
    transforms: &[Named],
) -> Option<Vec<String>> {
    let mut path: Vec<String> = capture
        .stack_frames(stack)
        .flat_map(|frame| naive_frame(capture, frame, granularity))
        .collect();

    if !invert {
        path.reverse();
    }

    for transform in transforms {
        match transform {
            Named::Focus(name) => {
                let first = path.iter().position(|step| step == name)?;
                path = path.split_off(first);
            }
            Named::Merge(name) => path.retain(|step| step != name),
            Named::Drop(name) => {
                if path.iter().any(|step| step == name) {
                    return None;
                }
            }
            Named::Collapse(name) => {
                let mut collapsed: Vec<String> = Vec::new();
                for step in path {
                    if !(step == *name && collapsed.last() == Some(&step)) {
                        collapsed.push(step);
                    }
                }

                path = collapsed;
            }
            Named::Cycles if invert => {
                path.reverse();
                path = loop_erased(&path);
                path.reverse();
            }
            Named::Cycles => path = loop_erased(&path),
        }
    }

    Some(path)
}

fn key_named(keys: &Keys<'_>, name: &str) -> Option<u32> {
    (0..index(keys.count())).find(|key| keys.name(*key) == name)
}

type Stacks = Vec<(Vec<usize>, bool)>;
type Samples = Vec<(u32, i64, u64, usize, u32)>;
type Waits = Vec<(i64, u64, usize, u32)>;

type Selection = (
    (
        Option<(i64, i64)>,
        Option<Vec<u32>>,
        Vec<(u8, prop::sample::Index)>,
        bool,
    ),
    (bool, Option<Vec<u32>>, bool),
    Granularity,
);

const FRAMES: usize = 10;

fn strategy() -> impl Strategy<Value = (Stacks, (Samples, Waits), Selection)> {
    let stacks = prop::collection::vec(
        (prop::collection::vec(0..FRAMES, 1..7), any::<bool>()),
        1..6,
    );
    stacks.prop_flat_map(|stacks| {
        let count = stacks.len();
        let samples = prop::collection::vec(
            (0..3_u32, 0..100_i64, 1..5_u64, 0..count, 0..=UNNAMED),
            0..40,
        );
        let waits = prop::collection::vec((0..100_i64, 1..5_u64, 0..count, 0..UNNAMED), 0..10);
        let range = prop::option::of((0..100_i64, 1..=100_i64))
            .prop_map(|range| range.map(|(start, length)| (start, start + length)));
        let threads = prop::option::of(prop::collection::vec(0..3_u32, 0..3));
        let transforms = prop::collection::vec((0..5_u8, any::<prop::sample::Index>()), 0..3);
        let tasks = prop::option::of(prop::collection::vec(0..=UNNAMED, 0..3));
        let granularity = prop::sample::select(vec![
            Granularity::Function,
            Granularity::PhysicalFunction,
            Granularity::Line,
            Granularity::Address,
        ]);
        (
            Just(stacks),
            (samples, waits),
            (
                (range, threads, transforms, any::<bool>()),
                (any::<bool>(), tasks, any::<bool>()),
                granularity,
            ),
        )
    })
}

fn selected(
    capture: &Capture,
    range: Option<(i64, i64)>,
    threads: Option<&Vec<u32>>,
    (waits, tasks): (bool, Option<&Vec<u32>>),
) -> Vec<(u32, u32, u64)> {
    let in_range = |time: i64| range.is_none_or(|(start, end)| start <= time && time < end);
    let task_kept =
        |task: u32| tasks.is_none_or(|tasks| tasks.iter().any(|choice| task_row(*choice) == task));
    let mut chosen = Vec::new();

    if waits {
        for wait in &capture.task_waits {
            if in_range(wait.time.get()) && task_kept(wait.task.get()) {
                chosen.push((wait.task.get(), wait.stack.get(), wait.weight.get()));
            }
        }

        return chosen;
    }

    for range_row in &capture.sample_ranges {
        if threads.is_some_and(|threads| !threads.contains(&range_row.thread.get())) {
            continue;
        }

        for sample in capture.thread_samples(range_row) {
            if in_range(sample.time.get()) && task_kept(sample.task.get()) {
                chosen.push((sample.task.get(), sample.stack.get(), sample.weight.get()));
            }
        }
    }

    chosen
}

proptest! {
    /// The engine agrees with a naive oracle that follows each sample
    /// alone: the same weight reaches every path of the tree and every
    /// key, under any selection and transforms of the keys the capture
    /// has, at any granularity, inverted or not.
    #[test]
    fn aggregation_agrees_with_a_sample_by_sample_oracle(
        (stacks, (samples, waits), ((range, threads, transforms, invert), (of_waits, tasks, by_task), granularity)) in strategy()
    ) {
        let capture = capture(&stacks, &samples, &waits);
        prop_assert_eq!(capture.validate(), Ok(()));
        let keys = Keys::new(&capture, granularity);

        let mut shown_keys: BTreeMap<String, u32> = BTreeMap::new();
        for frame in 0..index(capture.frames.len()) {
            for &key in keys.frame(frame) {
                let earlier = shown_keys.insert(shown(&keys, key), key);
                prop_assert!(earlier.is_none_or(|earlier| earlier == key), "{} is two keys", shown(&keys, key));
            }
        }

        let shown_keys: Vec<(String, u32)> = shown_keys.into_iter().collect();
        let mut query = Query {
            source: if of_waits { Source::Waits } else { Source::Samples },
            range,
            threads: threads.clone(),
            tasks: tasks.as_ref().map(|tasks| tasks.iter().map(|choice| task_row(*choice)).collect()),
            invert,
            by_task,
            ..Query::default()
        };

        let mut naive_transforms = Vec::new();

        for (kind, chosen) in &transforms {
            let (name, key) = chosen.get(&shown_keys).clone();
            let (named, transform) = match kind {
                0 => (Named::Focus(name), Transform::Focus(key)),
                1 => (Named::Merge(name), Transform::Merge(key)),
                2 => (Named::Drop(name), Transform::Drop(key)),
                3 => (Named::Collapse(name), Transform::CollapseRecursion(key)),
                _ => (Named::Cycles, Transform::CollapseCycles),
            };
            naive_transforms.push(named);
            query.transforms.push(transform);
        }

        let mut expected_totals: BTreeMap<Vec<String>, u64> = BTreeMap::new();
        let mut expected_functions: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        let mut expected_weight = 0;

        for (task, stack, weight) in selected(&capture, range, threads.as_ref(), (of_waits, tasks.as_ref())) {
            let Some(mut path) = naive_path(&capture, (stack, granularity), invert, &naive_transforms) else {
                continue;
            };

            if by_task {
                let choice = if task == NONE { UNNAMED } else { task };
                let group = format!("{} []", group_name(choice));
                if invert {
                    path.push(group);
                } else {
                    path.insert(0, group);
                }
            }

            expected_weight += weight;
            for depth in 1..=path.len() {
                *expected_totals.entry(path[..depth].to_vec()).or_default() += weight;
            }

            let innermost = if invert { path.first() } else { path.last() };
            if let Some(innermost) = innermost {
                expected_functions.entry(innermost.clone()).or_default().0 += weight;
            }

            let mut seen: Vec<&String> = Vec::new();
            for step in &path {
                if !seen.contains(&step) {
                    seen.push(step);
                    expected_functions.entry(step.clone()).or_default().1 += weight;
                }
            }
        }

        let tree = aggregate(&capture, &keys, &query);
        prop_assert_eq!(tree.weight, expected_weight);
        let mut totals: BTreeMap<Vec<String>, u64> = BTreeMap::new();

        for node in 0..tree.len() {
            let mut path = Vec::new();
            let mut at = index(node);
            while at != NONE {
                path.push(shown(&keys, tree.key[at as usize]));
                at = tree.parent[at as usize];
            }

            path.reverse();
            let earlier = totals.insert(path.clone(), tree.total[node]);
            prop_assert!(earlier.is_none(), "{:?} is two nodes", path);

            let end = tree.end[node] as usize;
            prop_assert!(end > node && end <= tree.len());
            prop_assert!((node + 1..end).all(|inner| tree.depth[inner] > tree.depth[node]));
            prop_assert!(end == tree.len() || tree.depth[end] <= tree.depth[node]);
        }

        prop_assert_eq!(totals, expected_totals);

        let costs = functions(&capture, &keys, &query);
        prop_assert_eq!(costs.weight, expected_weight);
        let costs: BTreeMap<String, (u64, u64)> = costs
            .rows
            .into_iter()
            .map(|cost| (shown(&keys, cost.key), (cost.own, cost.total)))
            .collect();
        let expected_functions: BTreeMap<String, (u64, u64)> = expected_functions
            .into_iter()
            .filter(|(_, (_, total))| *total > 0)
            .collect();
        prop_assert_eq!(costs, expected_functions);
    }
}

#[test]
fn density_buckets_kept_samples_by_lane_and_time() {
    let capture = capture(
        &[(vec![0], false), (vec![1], false)],
        &[
            (0, 0, 1, 0, 0),
            (0, 9, 2, 0, UNNAMED),
            (1, 5, 4, 1, 0),
            (1, 10, 8, 1, 0),
        ],
        &[(3, 7, 0, 1)],
    );
    let keys = Keys::new(&capture, Granularity::Function);
    let all = Query {
        range: Some((0, 10)),
        ..Query::default()
    };

    assert_eq!(
        density(&capture, &keys, &all, 2, Lanes::Threads),
        [(0, vec![1, 2]), (1, vec![0, 4])]
    );
    assert_eq!(
        density(&capture, &keys, &all, 2, Lanes::Tasks),
        [(0, vec![1, 4]), (NONE, vec![0, 2])]
    );
    let waits = Query {
        source: Source::Waits,
        ..all.clone()
    };

    assert_eq!(
        density(&capture, &keys, &waits, 2, Lanes::Threads),
        [(1, vec![7, 0])]
    );
    let without_b = Query {
        transforms: vec![Transform::Drop(key_named(&keys, "b").expect("b"))],
        ..all
    };

    assert_eq!(
        density(&capture, &keys, &without_b, 2, Lanes::Threads),
        [(0, vec![1, 2])]
    );
}

#[test]
fn off_cpu_time_falls_in_each_slice_it_overlaps() {
    let mut builder = CaptureBuilder::new();
    let (a, b) = (
        builder.thread(Pid::new(1), Tid::new(1), 0),
        builder.thread(Pid::new(1), Tid::new(2), 0),
    );
    builder.off_cpu(a, 5, 20, false, 0);
    builder.off_cpu(a, 30, 4, true, 0);
    builder.off_cpu(a, 45, 10, false, 0);
    builder.off_cpu(b, -5, 100, false, 0);

    let capture = builder.finish();
    let query = Query {
        range: Some((0, 40)),
        ..Query::default()
    };

    assert_eq!(
        off_cpu(&capture, &query, 4),
        [
            OffCpuLane {
                thread: a,
                blocked: vec![5, 10, 5, 0],
                preempted: vec![0, 0, 0, 4],
            },
            OffCpuLane {
                thread: b,
                blocked: vec![10; 4],
                preempted: vec![0; 4],
            },
        ]
    );

    let uneven = Query {
        range: Some((0, 10)),
        threads: Some(vec![b]),
        ..Query::default()
    };

    let lanes = off_cpu(&capture, &uneven, 3);
    assert_eq!(lanes.len(), 1);
    assert_eq!(lanes[0].blocked.iter().sum::<u64>(), 10, "{lanes:?}");
}

#[test]
fn the_analysis_stays_pure() {
    const FORBIDDEN: [&str; 11] = [
        "crate::backend",
        "crate::debug_info",
        "crate::profiler",
        "crate::sim",
        "nix::",
        "tokio",
        "std::fs",
        "std::env",
        "std::process",
        "std::thread",
        "std::time",
    ];
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/analysis");
    let mut checked = 0;

    for entry in std::fs::read_dir(&root).expect("read the analysis's sources") {
        let path = entry.expect("a directory entry").path();
        if path.file_name().is_some_and(|name| name == "tests.rs") {
            continue;
        }

        let source = std::fs::read_to_string(&path).expect("read a source file");
        let source = source.split("#[cfg(test)]").next().unwrap_or_default();

        for forbidden in FORBIDDEN {
            assert!(
                !source.contains(forbidden),
                "{} uses `{forbidden}`",
                path.display()
            );
        }

        checked += 1;
    }

    assert!(checked >= 3, "the analysis's sources were found");
}

fn ending_paths(tree: &CallTree, keys: &Keys<'_>) -> BTreeMap<Vec<String>, u64> {
    (0..tree.len())
        .filter(|node| tree.own[*node] > 0)
        .map(|node| {
            let mut path = Vec::new();
            let mut at = index(node);
            while at != NONE {
                path.push(keys.name(tree.key[at as usize]));
                at = tree.parent[at as usize];
            }

            path.reverse();
            (path, tree.own[node])
        })
        .collect()
}

#[test]
fn a_logical_stack_is_a_tasks_own_under_its_dispatch() {
    use crate::CodeRole::{Dispatch, Ordinary, Outermost, RuntimeInternal, Wrapper};
    let mut builder = CaptureBuilder::new();
    let module = builder.module("/bin/server", None, [0; 16], 0);
    let mut frame = |name: &str, role: crate::CodeRole| {
        let address = 0x100 * (builder.frames().len() as u64 + 1);
        let frame = builder.frame(FrameKind::Native, module, address, 0);
        builder.symbolize(
            frame,
            module,
            &[LineInput {
                role: crate::capture::records::role_number(role),
                ..line(name)
            }],
        );
        frame
    };

    let leaf = frame("decode", Ordinary);
    let pinned = frame("<Pin<Box<F>> as Future>::poll", Wrapper);
    let handler = frame("handler::{closure#0}", Ordinary);
    let core = frame("Core::poll", RuntimeInternal);
    let harness = frame("Harness::poll", Dispatch);
    let run = frame("run_task", RuntimeInternal);
    let start = frame("thread_start", Outermost);
    let park = frame("park", RuntimeInternal);
    let coop = frame("coop::budget", RuntimeInternal);
    let stacks = [
        builder.stack(&[leaf, pinned, handler, core, harness, run, start]),
        builder.stack(&[park, run, start]),
        builder.stack(&[coop, harness, run, start]),
    ];

    let thread = builder.thread(Pid::new(1), Tid::new(1), 0);
    for (time, stack) in stacks.into_iter().enumerate() {
        builder.sample(
            thread,
            i64::try_from(time).expect("a time"),
            1 << time,
            stack,
            NONE,
        );
    }

    let capture = builder.finish();
    let keys = Keys::new(&capture, Granularity::Function);
    let names = |path: &[&str]| {
        path.iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>()
    };

    let logical = aggregate(
        &capture,
        &keys,
        &Query {
            look: Look::Logical,
            ..Query::default()
        },
    );
    assert_eq!(
        ending_paths(&logical, &keys),
        BTreeMap::from([
            (
                names(&[
                    "Harness::poll",
                    "[task] handler::{closure#0}",
                    "handler::{closure#0}",
                    "decode"
                ]),
                1
            ),
            (names(&["thread_start", "run_task", "park"]), 2),
            (names(&["Harness::poll", "coop::budget"]), 4),
        ])
    );

    let native = aggregate(&capture, &keys, &Query::default());
    assert_eq!(
        ending_paths(&native, &keys)[&names(&[
            "thread_start",
            "run_task",
            "Harness::poll",
            "Core::poll",
            "handler::{closure#0}",
            "<Pin<Box<F>> as Future>::poll",
            "decode",
        ])],
        1
    );
}

fn python_capture() -> (Capture, u32, u32) {
    use crate::CodeRole::{Interpreter, Ordinary};
    let mut builder = CaptureBuilder::new();
    let module = builder.module("/bin/python3", None, [0; 16], 0);
    let mut native = |name: &str, role: crate::CodeRole| {
        let address = 0x100 * (builder.frames().len() as u64 + 1);
        let frame = builder.frame(FrameKind::Native, module, address, 0);
        builder.symbolize(
            frame,
            module,
            &[LineInput {
                role: crate::capture::records::role_number(role),
                ..line(name)
            }],
        );
        frame
    };

    let decode = native("decode", Ordinary);
    let call = native("cfunction_vectorcall_O", Interpreter);
    let evaluate = native("_PyEval_EvalFrameDefault", Interpreter);
    let vectorcall = native("PyObject_Vectorcall", Interpreter);
    let run = native("Py_RunMain", Interpreter);
    let main = native("main", Ordinary);
    let start = native("start_thread", Ordinary);
    let work = native("work", Ordinary);

    let mut python = |function: &str, line: u32, declared: u32| {
        builder.interpreted(&InterpretedPlace {
            function,
            file: Some("/srv/server.py"),
            line,
            declared: Some(("/srv/server.py", declared)),
        })
    };

    let handle = python("Server.handle", 12, 10);
    let serve = python("serve", 30, 28);
    let module_code = python("<module>", 40, 1);
    let mixed = builder.stack(&[
        decode,
        call,
        handle,
        serve,
        evaluate,
        vectorcall,
        module_code,
        evaluate,
        run,
        main,
    ]);
    let plain = builder.stack(&[work, start]);
    let thread = builder.thread(Pid::new(1), Tid::new(1), 0);
    builder.sample(thread, 0, 1, mixed, NONE);
    builder.sample(thread, 1, 2, plain, NONE);

    (builder.finish(), handle, decode)
}

#[test]
fn python_mixed_and_native_looks_of_one_stack() {
    let (capture, handle, decode) = python_capture();
    let names = |path: &[&str]| {
        path.iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>()
    };

    let keys = Keys::new(&capture, Granularity::Function);
    let look = |look: Look| {
        let query = Query {
            look,
            ..Query::default()
        };
        ending_paths(&aggregate(&capture, &keys, &query), &keys)
    };

    let plain = (names(&["start_thread", "work"]), 2);
    assert_eq!(
        look(Look::Python),
        BTreeMap::from([
            (names(&["<module>", "serve", "Server.handle", "decode"]), 1),
            plain.clone(),
        ])
    );
    assert_eq!(
        look(Look::Logical),
        BTreeMap::from([
            (
                names(&[
                    "main",
                    "Py_RunMain",
                    "_PyEval_EvalFrameDefault",
                    "<module>",
                    "PyObject_Vectorcall",
                    "_PyEval_EvalFrameDefault",
                    "serve",
                    "Server.handle",
                    "cfunction_vectorcall_O",
                    "decode",
                ]),
                1
            ),
            plain.clone(),
        ])
    );
    assert_eq!(
        look(Look::Native),
        BTreeMap::from([
            (
                names(&[
                    "main",
                    "Py_RunMain",
                    "_PyEval_EvalFrameDefault",
                    "PyObject_Vectorcall",
                    "_PyEval_EvalFrameDefault",
                    "cfunction_vectorcall_O",
                    "decode",
                ]),
                1
            ),
            plain,
        ])
    );

    let keys = Keys::new(&capture, Granularity::Address);
    let shown: Vec<String> = keys
        .frame(handle)
        .iter()
        .map(|key| keys.name(*key))
        .collect();
    assert_eq!(shown, ["Server.handle server.py:12"]);
    assert!(keys.interpreted(keys.frame(handle)[0]));
    assert!(!keys.interpreted(keys.frame(decode)[0]));
}

type Anonymous<'a> = (Option<&'a str>, Option<(&'a str, u32)>, u64);

fn anonymous(places: &[Anonymous<'_>]) -> (Capture, Vec<u32>) {
    let mut builder = CaptureBuilder::new();
    let thread = builder.thread(Pid::new(1), Tid::new(1), 0);
    let frames: Vec<u32> = places
        .iter()
        .map(|&(file, declared, _)| {
            builder.interpreted(&InterpretedPlace {
                function: "",
                file,
                line: file.map_or(0, |_| declared.map_or(1, |(_, line)| line + 3)),
                declared,
            })
        })
        .collect();
    for ((time, &frame), &(.., weight)) in (0..).zip(&frames).zip(places) {
        let stack = builder.stack(&[frame]);
        builder.sample(thread, time, weight, stack, NONE);
    }

    (builder.finish(), frames)
}

#[test]
fn a_function_of_a_remade_module_is_one_key() {
    let mut builder = CaptureBuilder::new();
    let thread = builder.thread(Pid::new(1), Tid::new(1), 0);
    let mut frame = |path: &str, digest: u8| {
        let module = builder.module(path, None, [digest; 16], 0x1000);
        let frame = builder.frame(FrameKind::Native, module, 0x10, 0);
        builder.symbolize(frame, module, &[line("JS:*lint")]);
        frame
    };
    let first = frame("[V8 code 0x7f0000000000]", 1);
    let again = frame("[V8 code 0x7f0000000000]", 2);
    let other = frame("[V8 code 0x7e0000000000]", 3);
    for (time, frame) in (0..).zip([first, again, other]) {
        let stack = builder.stack(&[frame]);
        builder.sample(thread, time, 1, stack, NONE);
    }
    let capture = builder.finish();
    let keys = Keys::new(&capture, Granularity::Function);
    let key = |frame: u32| keys.frame(frame)[0];

    assert_eq!(key(first), key(again));
    assert_ne!(key(first), key(other));
    assert_eq!(keys.named("JS:*lint").len(), 2);
}

#[test]
fn functions_that_share_a_name_each_have_an_exact_name() {
    let mut builder = CaptureBuilder::new();
    let thread = builder.thread(Pid::new(1), Tid::new(1), 0);
    let mut function = |name, file, line| {
        builder.interpreted(&InterpretedPlace {
            function: name,
            file: Some(file),
            line: line + 1,
            declared: Some((file, line)),
        })
    };
    let lint = function("lint", "/app/linter.js", 10);
    let first = function("create", "/app/rules/first.js", 3);
    let second = function("create", "/app/rules/second.js", 7);
    for (time, frame) in (0..).zip([first, second]) {
        let stack = builder.stack(&[frame, lint]);
        builder.sample(thread, time, 1, stack, NONE);
    }
    let capture = builder.finish();
    let keys = Keys::new(&capture, Granularity::Function);
    let key = |frame: u32| keys.frame(frame)[0];

    assert_eq!(keys.named("create").len(), 2);
    assert_eq!(keys.exact_name(key(lint)), "lint");
    for (frame, exact) in [
        (first, "create [/app/rules/first.js:3]"),
        (second, "create [/app/rules/second.js:7]"),
    ] {
        assert_eq!(keys.exact_name(key(frame)), exact);
        assert_eq!(keys.named(exact), [key(frame)], "{exact}");
    }
}

#[test]
fn an_anonymous_function_is_named_for_where_it_is_declared() {
    let linter = Some(("/app/lib/linter.js", 557));
    let loader = "node:internal/modules/cjs/loader";
    let (current, frames) = anonymous(&[
        (Some("/app/lib/linter.js"), linter, 4),
        (Some(loader), Some((loader, 1490)), 1),
        (Some("/app/x.js"), Some(("/app/x.js", 0)), 1),
        (None, None, 1),
        (None, linter, 1),
    ]);
    let keys = Keys::new(&current, Granularity::Function);
    let key = |frame: u32| keys.frame(frame)[0];
    let shown: Vec<String> = frames.iter().map(|&frame| keys.name(key(frame))).collect();
    assert_eq!(
        shown,
        [
            "<anonymous linter.js:557>",
            "<anonymous node:internal/modules/cjs/loader:1490>",
            "<anonymous x.js>",
            "<anonymous>",
            "<anonymous linter.js:557>",
        ]
    );
    assert_eq!(
        key(frames[0]),
        key(frames[4]),
        "compiled code is its function's"
    );
    assert_eq!(keys.named(""), Vec::<u32>::new());
    assert_eq!(keys.named("<anonymous linter.js:557>"), [key(frames[0])]);
    let root = keys.task(key(frames[0])).expect("a task root");
    assert_eq!(keys.name(root), "[task] <anonymous linter.js:557>");

    let lines = Keys::new(&current, Granularity::Line);
    assert_eq!(
        lines.name(lines.frame(frames[0])[0]),
        "<anonymous linter.js:557> linter.js:560"
    );

    let mut builder = CaptureBuilder::new();
    let module = builder.module("/bin/app", None, [0; 16], 0);
    let frame = builder.frame(FrameKind::Native, module, 0x100, 0);
    builder.symbolize(frame, module, &[line("")]);
    let native = builder.finish();
    let native_keys = Keys::new(&native, Granularity::Function);
    assert_eq!(native_keys.name(native_keys.frame(frame)[0]), "");

    let (base, _) = anonymous(&[
        (Some("/app/lib/other.js"), Some(("/app/lib/other.js", 7)), 2),
        (Some("/app/lib/linter.js"), linter, 3),
    ]);
    let base_keys = Keys::new(&base, Granularity::Function);
    let everything = Query {
        look: Look::Logical,
        ..Query::default()
    };
    let mut paired: Vec<(String, u64, u64)> = diff_costs(
        (&keys, &keys.functions(&everything)),
        (&base_keys, &base_keys.functions(&everything)),
    )
    .into_iter()
    .map(|cost| {
        let name = match cost.side {
            Side::Current => keys.name(cost.key),
            Side::Base => base_keys.name(cost.key),
        };
        (name, cost.own, cost.base_own)
    })
    .collect();
    paired.sort();
    let expected = [
        ("<anonymous linter.js:557>", 5, 3),
        ("<anonymous node:internal/modules/cjs/loader:1490>", 1, 0),
        ("<anonymous other.js:7>", 0, 2),
        ("<anonymous x.js>", 1, 0),
        ("<anonymous>", 1, 0),
    ]
    .map(|(name, own, base_own)| (name.to_owned(), own, base_own));
    assert_eq!(paired, expected);
}

fn named_stacks(stacks: &[(&[&str], u64)]) -> Capture {
    let mut builder = CaptureBuilder::new();
    let module = builder.module("/bin/app", None, [0; 16], 0);
    let thread = builder.thread(Pid::new(1), Tid::new(1), 0);
    let mut frames: BTreeMap<String, u32> = BTreeMap::new();
    for (time, &(names, weight)) in (0..).zip(stacks) {
        let innermost_first: Vec<u32> = names
            .iter()
            .rev()
            .map(|name| {
                let next = 0x100 * (frames.len() as u64 + 1);
                *frames.entry((*name).to_owned()).or_insert_with(|| {
                    let frame = builder.frame(FrameKind::Native, module, next, 0);
                    builder.symbolize(frame, module, &[line(name)]);
                    frame
                })
            })
            .collect();
        let stack = builder.stack(&innermost_first);
        builder.sample(thread, time, weight, stack, NONE);
    }

    builder.finish()
}

#[test]
fn collapsing_cycles_cuts_each_path_back_to_the_first_call() {
    let capture = named_stacks(&[
        (&["main", "self", "self", "self", "work"], 1),
        (
            &["main", "visit", "children", "visit", "children", "work"],
            2,
        ),
        (&["a", "b", "c", "a", "b", "c", "work"], 4),
        (&["a", "b", "a", "c", "a"], 8),
        (&["main", "visit", "children", "visit"], 16),
    ]);
    let keys = Keys::new(&capture, Granularity::Function);
    let path = |names: &[&str]| {
        names
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>()
    };
    let paths = |transforms: Vec<Transform>, invert: bool| {
        let query = Query {
            transforms,
            invert,
            ..Query::default()
        };
        ending_paths(&aggregate(&capture, &keys, &query), &keys)
    };

    assert_eq!(
        paths(vec![Transform::CollapseCycles], false),
        BTreeMap::from([
            (path(&["main", "self", "work"]), 1),
            (path(&["main", "visit", "children", "work"]), 2),
            (path(&["a", "b", "c", "work"]), 4),
            (path(&["a"]), 8),
            (path(&["main", "visit"]), 16),
        ])
    );

    let costs = |transforms: Vec<Transform>| {
        let query = Query {
            transforms,
            ..Query::default()
        };
        functions(&capture, &keys, &query)
            .rows
            .into_iter()
            .map(|cost| (keys.name(cost.key), (cost.own, cost.total)))
            .collect::<BTreeMap<_, _>>()
    };
    let collapsed = costs(vec![Transform::CollapseCycles]);
    let plain = costs(Vec::new());
    assert_eq!(collapsed["main"], (0, 19));
    assert_eq!(collapsed["visit"], (16, 18));
    assert_eq!(collapsed["a"], (8, 12));
    for root in ["main", "a"] {
        assert_eq!(collapsed[root].1, plain[root].1, "{root}");
    }
    assert_eq!((plain["b"].1, collapsed["b"].1), (12, 4));

    assert_eq!(
        paths(vec![Transform::CollapseCycles], true)[&path(&["work", "c", "b", "a"])],
        4
    );

    let self_key = keys.named("self")[0];
    let one = paths(vec![Transform::CollapseRecursion(self_key)], false);
    assert_eq!(one[&path(&["main", "self", "work"])], 1);
    assert_eq!(
        one[&path(&["main", "visit", "children", "visit", "children", "work"])],
        2
    );
}

#[test]
fn a_name_without_its_parameters_finds_the_function() {
    let capture = named_stacks(&[(
        &[
            "main",
            "content::RendererMain(content::MainFunctionParams)",
            "blink::Paint(int) const",
            "blink::Paint(float) const",
            "blink::PaintLayer::Update()",
            "operator()",
        ],
        1,
    )]);
    let keys = Keys::new(&capture, Granularity::Function);
    let name = |key: u32| keys.name(key);

    let renderer = keys.named("content::RendererMain");
    assert_eq!(
        renderer.iter().map(|&key| name(key)).collect::<Vec<_>>(),
        ["content::RendererMain(content::MainFunctionParams)"]
    );
    let mut paints: Vec<String> = keys.named("blink::Paint").into_iter().map(name).collect();
    paints.sort();
    assert_eq!(
        paints,
        ["blink::Paint(float) const", "blink::Paint(int) const"]
    );

    assert_eq!(keys.named("operator()").len(), 1);
    assert!(keys.named("blink::PaintLayer").is_empty());
    assert!(keys.named("content::Renderer").is_empty());

    let similar: Vec<String> = keys
        .similar("paintlayer", 5)
        .into_iter()
        .map(name)
        .collect();
    assert_eq!(similar, ["blink::PaintLayer::Update()"]);
    assert!(keys.similar("nothing like it", 5).is_empty());
}

type Sample = (u32, i64, u64, u32);

fn labeled(spans: &[(u32, i64, i64, u32)], samples: &[Sample]) -> Capture {
    labeled_off(spans, samples, &[])
}

type Off = (u32, i64, u64, bool);

fn labeled_off(spans: &[(u32, i64, i64, u32)], samples: &[Sample], off: &[Off]) -> Capture {
    let mut builder = CaptureBuilder::new();
    let module = builder.module("/bin/app", None, [0; 16], 0);
    let frame = builder.frame(FrameKind::Native, module, 0x10, 0);
    builder.symbolize(frame, module, &[line("a")]);
    let stack = builder.stack(&[frame]);
    let threads = [
        builder.thread(Pid::new(1), Tid::new(1), 0),
        builder.thread(Pid::new(1), Tid::new(2), 0),
    ];

    let tasks: Vec<u32> = (1..=3)
        .map(|number| builder.task("goroutine", number, 0))
        .collect();
    builder.describe_task(tasks[1], NONE, Some("phase=spin"));

    let site = builder.site(
        "route",
        "app.rs",
        1,
        module,
        microscope_probes::abi::SITE_LABEL,
    );

    for &(thread, start, length, value) in spans {
        let value = builder.string(&format!("v{value}"));
        builder.zone(
            threads[thread as usize],
            crate::capture::records::Zone {
                start: start.into(),
                end: (start + length).into(),
                site: site.into(),
                flags: 0.into(),
                value: value.into(),
                reserved: 0.into(),
            },
        );
    }

    for &(thread, time, weight, task) in samples {
        builder.sample(
            threads[thread as usize],
            time,
            weight,
            stack,
            task_row(task),
        );
    }

    for &(thread, time, duration, preempted) in off {
        builder.off_cpu(threads[thread as usize], time, duration, preempted, 0);
    }

    builder.finish()
}

fn samples_carry_the_open_labels(
    spans: &[(u32, i64, i64, u32)],
    samples: &[Sample],
    wanted: u32,
) -> Result<(), TestCaseError> {
    let capture = labeled(spans, samples);
    let keys = Keys::new(&capture, Granularity::Function);

    let route = |&(thread, time, _, _): &Sample| {
        spans
            .iter()
            .enumerate()
            .filter(|(_, (on, start, length, _))| {
                *on == thread && *start <= time && time < start + length
            })
            .max_by_key(|(order, (_, start, length, _))| {
                (
                    *start,
                    std::cmp::Reverse(*length),
                    std::cmp::Reverse(*order),
                )
            })
            .map(|(_, (_, _, _, value))| *value)
    };

    let carries = |sample: &Sample, value: u32| {
        spans.iter().any(|(on, start, length, carried)| {
            *on == sample.0 && *start <= sample.1 && sample.1 < start + length && *carried == value
        })
    };

    let weight = |keep: &dyn Fn(&Sample) -> bool| -> u64 {
        samples
            .iter()
            .filter(|sample| keep(sample))
            .map(|sample| sample.2)
            .sum()
    };

    let query = |labels: Vec<(&str, String)>, by: Option<&str>| Query {
        labels: labels
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
        by_label: by.map(str::to_owned),
        ..Query::default()
    };

    let routed = keys.aggregate(&query(vec![("route", format!("v{wanted}"))], None));
    prop_assert_eq!(routed.weight, weight(&|sample| carries(sample, wanted)));
    let spun = keys.aggregate(&query(vec![("phase", "spin".to_owned())], None));
    prop_assert_eq!(spun.weight, weight(&|sample| sample.3 == 1));
    let by = keys.aggregate(&query(Vec::new(), Some("route")));
    let mut roots = BTreeMap::new();

    for node in (0..by.len()).filter(|&node| by.depth[node] == 0) {
        roots.insert(keys.name(by.key[node]), by.total[node]);
    }

    let mut expected = BTreeMap::new();

    for sample in samples {
        let name = route(sample).map_or_else(
            || "[no route]".to_owned(),
            |value| format!("route=v{value}"),
        );
        *expected.entry(name).or_insert(0) += sample.2;
    }

    if spans.is_empty() {
        expected = expected
            .into_values()
            .map(|weight| ("[no such label]".to_owned(), weight))
            .collect();
    }

    prop_assert_eq!(roots, expected);
    Ok(())
}

#[test]
fn of_two_alike_spans_the_first_recorded_is_innermost() {
    samples_carry_the_open_labels(&[(1, 35, 27, 0), (1, 35, 27, 1)], &[(1, 35, 1, 0)], 0).unwrap();
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn samples_carry_the_labels_open_on_their_thread_and_their_tasks(
        spans in proptest::collection::vec((0..2_u32, 0..100_i64, 0..50_i64, 0..3_u32), 0..12),
        samples in proptest::collection::vec((0..2_u32, 0..160_i64, 1..5_u64, 0..4_u32), 0..40),
        wanted in 0..3_u32,
    ) {
        samples_carry_the_open_labels(&spans, &samples, wanted)?;
    }

    #[test]
    fn a_scope_keeps_the_samples_inside_its_zones_and_nowhere_else(
        spans in proptest::collection::vec((0..2_u32, 0..100_i64, 0..50_i64, 0..3_u32), 0..12),
        samples in proptest::collection::vec((0..2_u32, 0..160_i64, 1..5_u64, 0..4_u32), 0..40),
        band in proptest::option::of((0..40_u64, 0..40_u64)),
        outside: bool,
    ) {
        let band = band.map(|(low, more)| (low, low + more));
        a_scope_keeps_the_samples_inside_its_zones(&spans, &samples, band, outside)?;
    }

    /// A track's blocks draw every zone in the range once, fold only zones
    /// narrower than the fold, and never overlap in a row; a zone drawn
    /// alone is one that was recorded, below every zone it begins inside,
    /// however the zones cross.
    /// However zones fold, a thread's blocks hold exactly the time its
    /// zones in the range cover, and of that, exactly the time it spent
    /// blocked and preempted.
    #[test]
    fn zone_blocks_hold_their_zones_time_off_the_cpu(
        spans in proptest::collection::vec((0..2_u32, 0..400_i64, 0..60_i64, 0..3_u32), 0..30),
        gaps in proptest::collection::vec((0..2_u32, 0..40_i64, 1..40_u64, any::<bool>()), 0..30),
        range in (0..300_i64, 1..300_i64),
        fold in 1..40_i64,
    ) {
        let mut at = [0_i64; 2];
        let off: Vec<Off> = gaps
            .iter()
            .map(|&(thread, gap, length, preempted)| {
                let time = at[thread as usize] + gap;
                at[thread as usize] = time + length.cast_signed();
                (thread, time, length, preempted)
            })
            .collect();
        let capture = labeled_off(&spans, &[], &off);
        let range = (range.0, range.0 + range.1);
        let blocks = zone_blocks(&capture, range, None, fold, None);

        let mut drawn: BTreeMap<u32, (i64, i64, i64)> = BTreeMap::new();
        for block in &blocks {
            prop_assert!(block.blocked + block.preempted <= block.covered, "{:?}", block);
            let totals = drawn.entry(block.thread).or_default();
            *totals = (totals.0 + block.covered, totals.1 + block.blocked, totals.2 + block.preempted);
        }

        let mut expected: BTreeMap<u32, (i64, i64, i64)> = BTreeMap::new();
        for &(thread, start, length, _) in &spans {
            if start + length > range.0 && start < range.1 {
                let totals = expected.entry(thread).or_default();
                totals.0 += length;
                for &(on, time, duration, preempted) in &off {
                    let overlap = ((time + duration.cast_signed()).min(start + length) - time.max(start)).max(0);
                    if on == thread && preempted {
                        totals.2 += overlap;
                    } else if on == thread {
                        totals.1 += overlap;
                    }
                }
            }
        }

        let rows: BTreeMap<u32, (i64, i64, i64)> = drawn
            .into_iter()
            .map(|(row, totals)| (capture.threads[row as usize].tid.get() - 1, totals))
            .collect();
        prop_assert_eq!(rows, expected);
    }

    /// A run never mixes chosen zones with others, so the chosen ones are
    /// marked at every width: the blocks marked hold exactly the zones of a
    /// length in the band, and the rest exactly the others.
    #[test]
    fn zone_blocks_fold_chosen_zones_only_with_each_other(
        spans in proptest::collection::vec((0..2_u32, 0..400_i64, 0..60_i64, 0..3_u32), 0..40),
        range in (0..300_i64, 1..300_i64),
        fold in 1..400_i64,
        band in (0..60_u64, 0..60_u64),
    ) {
        let capture = labeled(&spans, &[]);
        let range = (range.0, range.0 + range.1);
        let band = (band.0.min(band.1), band.0.max(band.1));
        let sites = sites_named(&capture, "route");
        let chosen = ChosenZones { sites: &sites, band: Some(band) };
        let blocks = zone_blocks(&capture, range, None, fold, Some(chosen));

        let marked: u32 = blocks.iter().filter(|block| block.chosen).map(|block| block.count).sum();
        let rest: u32 = blocks.iter().filter(|block| !block.chosen).map(|block| block.count).sum();
        let (mut inside, mut outside) = (0, 0);
        for &(_, start, length, _) in &spans {
            if start + length > range.0 && start < range.1 {
                if (band.0..=band.1).contains(&length.cast_unsigned()) {
                    inside += 1;
                } else {
                    outside += 1;
                }
            }
        }
        prop_assert_eq!((marked, rest), (inside, outside));

        let none = ChosenZones { sites: &[], band: None };
        let blocks = zone_blocks(&capture, range, None, fold, Some(none));
        prop_assert!(blocks.iter().all(|block| !block.chosen));
    }

    #[test]
    fn zone_blocks_draw_each_zone_once_and_fold_only_narrow_ones(
        spans in proptest::collection::vec((0..2_u32, 0..400_i64, 0..60_i64, 0..3_u32), 0..40),
        range in (0..300_i64, 1..300_i64),
        fold in 1..40_i64,
    ) {
        let capture = labeled(&spans, &[]);
        let range = (range.0, range.0 + range.1);
        let blocks = zone_blocks(&capture, range, None, fold, None);
        let mut drawn: BTreeMap<u32, u32> = BTreeMap::new();

        for pair in blocks.windows(2) {
            if (pair[0].thread, pair[0].depth) == (pair[1].thread, pair[1].depth) {
                prop_assert!(pair[0].end <= pair[1].start, "{:?}", pair);
            }
        }

        for block in &blocks {
            *drawn.entry(block.thread).or_default() += block.count;
            prop_assert!(block.count == 1 || block.end - block.start <= fold, "{:?}", block);
        }

        let mut expected: BTreeMap<u32, u32> = BTreeMap::new();
        let mut recorded: Vec<(u32, i64, i64, String)> = Vec::new();
        for &(thread, start, length, value) in &spans {
            if start + length > range.0 && start < range.1 {
                *expected.entry(thread).or_default() += 1;
                recorded.push((thread, start, start + length, format!("v{value}")));
            }
        }

        prop_assert_eq!(drawn, expected);

        let alone: Vec<_> = blocks.iter().filter(|block| block.count == 1).collect();
        for block in &alone {
            let zone = (block.thread, block.start, block.end, capture.string(block.value).to_owned());
            let found = recorded.iter().position(|recorded| *recorded == zone);
            prop_assert!(found.is_some(), "{:?} was not recorded", zone);
            recorded.swap_remove(found.unwrap_or_default());
        }

        for outer in &alone {
            for inner in alone.iter().filter(|inner| inner.thread == outer.thread) {
                let first = (outer.start, std::cmp::Reverse(outer.end)) < (inner.start, std::cmp::Reverse(inner.end));
                if first && inner.start < outer.end {
                    prop_assert!(outer.depth < inner.depth, "{:?} holds {:?}", outer, inner);
                }
            }
        }
    }
}

fn a_scope_keeps_the_samples_inside_its_zones(
    spans: &[(u32, i64, i64, u32)],
    samples: &[Sample],
    band: Option<(u64, u64)>,
    outside: bool,
) -> Result<(), TestCaseError> {
    let capture = labeled(spans, samples);
    let keys = Keys::new(&capture, Granularity::Function);
    let sites = sites_named(&capture, "route");
    let in_band =
        |length: i64| band.is_none_or(|(low, high)| (low..=high).contains(&length.unsigned_abs()));

    let scope = ZoneScope {
        sites: sites.clone(),
        band,
        outside,
    };
    let query = Query {
        zones: Some(scope.clone()),
        ..Query::default()
    };
    let expected: u64 = samples
        .iter()
        .filter(|&&(thread, time, _, _)| {
            spans.iter().any(|&(on, start, length, _)| {
                on == thread && start <= time && time < start + length && in_band(length) != outside
            })
        })
        .map(|sample| sample.2)
        .sum();
    prop_assert_eq!(keys.aggregate(&query).weight, expected);

    let waits = Query {
        source: Source::Waits,
        zones: Some(scope),
        ..Query::default()
    };
    prop_assert_eq!(keys.aggregate(&waits).weight, 0);

    let stats = zone_stats(&capture, &sites, ((0, 1_000), None), band, Some(20));
    let lengths: Vec<i64> = spans.iter().map(|span| span.2).collect();
    let banded: Vec<i64> = lengths
        .iter()
        .copied()
        .filter(|&length| in_band(length))
        .collect();
    prop_assert_eq!(
        stats.banded,
        (
            banded.len() as u64,
            banded.iter().sum::<i64>().unsigned_abs()
        )
    );
    prop_assert_eq!(
        stats.shorter,
        lengths.iter().filter(|&&length| length < 20).count() as u64
    );
    prop_assert!(
        stats
            .longest
            .iter()
            .all(|zone| in_band(zone.end - zone.start))
    );
    prop_assert_eq!(stats.longest.len(), banded.len().min(10));
    Ok(())
}

#[test]
fn a_plot_keeps_each_bins_extremes_and_counts_what_it_cannot_draw() {
    let mut builder = CaptureBuilder::new();
    let module = builder.module("/bin/app", None, [0; 16], 0);
    let (one, two) = (
        builder.thread(Pid::new(1), Tid::new(1), 0),
        builder.thread(Pid::new(1), Tid::new(2), 0),
    );
    let kind = microscope_probes::abi::SITE_PLOT;
    let sites = [
        builder.site("depth", "a.rs", 1, module, kind),
        builder.site("depth", "b.rs", 2, module, kind),
        builder.site("rate", "a.rs", 3, module, kind),
    ];

    for (thread, site, time, value) in [
        (one, 0, 5, 3.0),
        (two, 1, 1, 7.0),
        (one, 0, 9, -2.0),
        (two, 1, 12, f64::NAN),
        (one, 0, 15, 4.0),
        (one, 2, 30, 1.0),
        (one, 0, 40, 9.0),
    ] {
        builder.plot(thread, sites[site], time, value);
    }

    let capture = builder.finish();
    let series = plot_series(&capture, (0, 20), None, 2);
    let named: Vec<&str> = series
        .iter()
        .map(|series| capture.string(series.name))
        .collect();
    assert_eq!(named, ["depth"]);
    let depth = &series[0];
    assert_eq!((depth.values, depth.unshown), (4, 1));
    let bins: Vec<(i64, f64, f64, f64, f64)> = depth
        .bins
        .iter()
        .map(|bin| (bin.time, bin.first, bin.last, bin.low, bin.high))
        .collect();
    assert_eq!(bins, [(1, 7.0, -2.0, -2.0, 7.0), (15, 4.0, 4.0, 4.0, 4.0)]);
    assert_eq!((depth.held, depth.continues), (None, true));
    let only = plot_series(&capture, (0, 50), Some(&[two]), 1);
    assert_eq!((only.len(), only[0].values, only[0].unshown), (1, 1, 1));

    let between = plot_series(&capture, (20, 35), None, 1);
    let held: Vec<(&str, Option<f64>, bool, usize)> = between
        .iter()
        .map(|series| {
            let name = capture.string(series.name);
            (name, series.held, series.continues, series.bins.len())
        })
        .collect();
    assert_eq!(
        held,
        [("depth", Some(4.0), true, 0), ("rate", None, false, 1)]
    );

    let after_nan = plot_series(&capture, (10, 14), None, 1);
    assert_eq!(
        (
            after_nan[0].held,
            after_nan[0].unshown,
            after_nan[0].bins.len()
        ),
        (Some(-2.0), 1, 0)
    );
}

#[test]
fn a_plot_of_no_site_is_read() {
    let mut capture = labeled(&[(0, 0, 10, 1)], &[(0, 5, 1, UNNAMED)]);
    capture.plots.push(crate::capture::records::Plot {
        time: 5.into(),
        value: 1.0_f64.to_bits().into(),
        site: NONE.into(),
        thread: NONE.into(),
    });
    assert_eq!(capture.validate(), Ok(()));

    let series = plot_series(&capture, (0, 10), None, 1);
    assert_eq!(
        series
            .iter()
            .map(|series| (series.name, series.values))
            .collect::<Vec<_>>(),
        [(NONE, 1)]
    );
}

const NAMED: [(&str, &str); 6] = [
    ("a", "/bin/app"),
    ("b", "/bin/app"),
    ("c", "/bin/app"),
    ("a", "/lib/libz.so"),
    ("d", "/lib/libz.so"),
    ("b", "/lib/libz.so"),
];

type WeightedStack = (Vec<usize>, u64);

fn stacks_of(samples: &[WeightedStack], order: &[usize]) -> Capture {
    let mut builder = CaptureBuilder::new();
    let thread = builder.thread(Pid::new(1), Tid::new(1), 0);
    let mut frames = [NONE; NAMED.len()];

    for &at in order {
        let (name, path) = NAMED[at];
        let module = builder.module(path, None, [0; 16], 0);
        let frame = builder.frame(FrameKind::Native, module, 0x10 * at as u64, 0);
        builder.symbolize(frame, module, &[line(name)]);
        frames[at] = frame;
    }

    for (time, (stack, weight)) in (0..).zip(samples) {
        let stack: Vec<u32> = stack.iter().map(|&at| frames[at]).collect();
        let stack = builder.stack(&stack);
        builder.sample(thread, time, *weight, stack, NONE);
    }

    builder.finish()
}

fn path_weights(samples: &[WeightedStack]) -> BTreeMap<Vec<&'static str>, (u64, u64)> {
    let mut paths: BTreeMap<Vec<&'static str>, (u64, u64)> = BTreeMap::new();
    for (stack, weight) in samples {
        let path: Vec<&str> = stack.iter().rev().map(|&at| NAMED[at].0).collect();
        paths.entry(path.clone()).or_default().0 += weight;
        for depth in 1..=path.len() {
            paths.entry(path[..depth].to_vec()).or_default().1 += weight;
        }
    }

    paths
}

fn name_costs(samples: &[WeightedStack]) -> BTreeMap<&'static str, (u64, u64)> {
    let mut costs: BTreeMap<&'static str, (u64, u64)> = BTreeMap::new();
    for (stack, weight) in samples {
        costs.entry(NAMED[stack[0]].0).or_default().0 += weight;
        let mut functions = stack.clone();
        functions.sort_unstable();
        functions.dedup();
        for at in functions {
            costs.entry(NAMED[at].0).or_default().1 += weight;
        }
    }

    costs
}

fn named_functions(samples: &[WeightedStack]) -> BTreeMap<&'static str, usize> {
    let called: std::collections::BTreeSet<usize> = samples
        .iter()
        .flat_map(|(stack, _)| stack.iter().copied())
        .collect();
    let mut count = BTreeMap::new();
    for at in called {
        *count.entry(NAMED[at].0).or_default() += 1;
    }

    count
}

fn weighted_stacks() -> impl Strategy<Value = Vec<WeightedStack>> {
    proptest::collection::vec(
        (proptest::collection::vec(0..NAMED.len(), 1..5), 1..100_u64),
        0..20,
    )
}

fn order() -> impl Strategy<Value = Vec<usize>> {
    Just((0..NAMED.len()).collect::<Vec<_>>()).prop_shuffle()
}

#[expect(
    clippy::too_many_lines,
    reason = "each check reads against the two profiles built above it"
)]
fn a_diff_holds_each_profiles_weight(
    (ours, theirs): (&[WeightedStack], &[WeightedStack]),
    (our_order, their_order): (&[usize], &[usize]),
) -> Result<(), TestCaseError> {
    let (current, base) = (stacks_of(ours, our_order), stacks_of(theirs, their_order));
    let (current_keys, base_keys) = (
        Keys::new(&current, Granularity::Function),
        Keys::new(&base, Granularity::Function),
    );
    let everything = Query::default();
    let diff = diff_trees(
        (&current_keys, &current_keys.aggregate(&everything)),
        (&base_keys, &base_keys.aggregate(&everything)),
    );
    let name = |node: usize| match diff.side[node] {
        Side::Current => current_keys.name(diff.key[node]),
        Side::Base => base_keys.name(diff.key[node]),
    };

    let mut paths: BTreeMap<Vec<&'static str>, [(u64, u64); 2]> = BTreeMap::new();

    for node in 0..diff.len() {
        let mut path = Vec::new();
        let mut at = index(node);
        while at != NONE {
            path.push(name(at as usize));
            at = diff.parent[at as usize];
        }

        path.reverse();
        let shown: Vec<&'static str> = path
            .iter()
            .map(|name| {
                NAMED
                    .iter()
                    .find(|(known, _)| known == name)
                    .expect("a function")
                    .0
            })
            .collect();
        let weights = paths.entry(shown).or_default();
        weights[0].0 += diff.own[node];
        weights[0].1 += diff.total[node];
        weights[1].0 += diff.base_own[node];
        weights[1].1 += diff.base_total[node];

        let inner =
            (node + 1..diff.end[node] as usize).filter(|&child| diff.parent[child] == index(node));
        let (total, base_total) = inner
            .fold((diff.own[node], diff.base_own[node]), |sum, child| {
                (sum.0 + diff.total[child], sum.1 + diff.base_total[child])
            });
        prop_assert_eq!(
            (diff.total[node], diff.base_total[node]),
            (total, base_total)
        );
        prop_assert!(
            (node + 1..diff.end[node] as usize).all(|inside| diff.depth[inside] > diff.depth[node])
        );
    }

    let mut functions_before: BTreeMap<Vec<usize>, (u64, u64)> = BTreeMap::new();
    for (stack, weight) in theirs {
        let path: Vec<usize> = stack.iter().rev().copied().collect();
        functions_before.entry(path.clone()).or_default().0 += weight;
        for depth in 1..=path.len() {
            functions_before
                .entry(path[..depth].to_vec())
                .or_default()
                .1 += weight;
        }
    }

    let function = |key: u32| {
        let name = current_keys.name(key);
        let module = current_keys.module(key).unwrap_or_default();
        NAMED
            .iter()
            .position(|(known, path)| *known == name && path.ends_with(module))
            .expect("a function")
    };

    for node in (0..diff.len()).filter(|&node| diff.side[node] == Side::Current) {
        let mut path = Vec::new();
        let mut at = index(node);
        while at != NONE {
            path.push(function(diff.key[at as usize]));
            at = diff.parent[at as usize];
        }

        path.reverse();
        if let Some(&weights) = functions_before.get(&path) {
            prop_assert_eq!(
                (diff.base_own[node], diff.base_total[node]),
                weights,
                "{:?}",
                path
            );
        }
    }

    let side = |side: usize| -> BTreeMap<Vec<&'static str>, (u64, u64)> {
        paths
            .iter()
            .map(|(path, weights)| (path.clone(), weights[side]))
            .filter(|(_, weights)| *weights != (0, 0))
            .collect()
    };
    prop_assert_eq!(side(0), path_weights(ours));
    prop_assert_eq!(side(1), path_weights(theirs));

    let costs = diff_costs(
        (&current_keys, &current_keys.functions(&everything)),
        (&base_keys, &base_keys.functions(&everything)),
    );

    let mut rows: BTreeMap<String, usize> = BTreeMap::new();
    let mut summed: [BTreeMap<String, (u64, u64)>; 2] = Default::default();
    let mut current_rows = Vec::new();

    let mut costs_before: BTreeMap<usize, (u64, u64)> = BTreeMap::new();
    for (stack, weight) in theirs {
        costs_before.entry(stack[0]).or_default().0 += weight;
        let mut called = stack.clone();
        called.sort_unstable();
        called.dedup();
        for at in called {
            costs_before.entry(at).or_default().1 += weight;
        }
    }

    for cost in costs {
        let name = match cost.side {
            Side::Current => {
                current_rows.push(cost.key);
                if let Some(&before) = costs_before.get(&function(cost.key)) {
                    prop_assert_eq!(
                        (cost.base_own, cost.base_total),
                        before,
                        "{}",
                        current_keys.qualified(cost.key)
                    );
                }

                current_keys.name(cost.key)
            }
            Side::Base => base_keys.name(cost.key),
        };

        *rows.entry(name.clone()).or_default() += 1;
        let now = summed[0].entry(name.clone()).or_default();
        (now.0, now.1) = (now.0 + cost.own, now.1 + cost.total);
        let before = summed[1].entry(name).or_default();
        (before.0, before.1) = (before.0 + cost.base_own, before.1 + cost.base_total);
    }

    let named = |costs: BTreeMap<&str, (u64, u64)>| -> BTreeMap<String, (u64, u64)> {
        costs
            .into_iter()
            .map(|(name, cost)| (name.to_owned(), cost))
            .collect()
    };
    let [now, before] = summed.map(|side| {
        side.into_iter()
            .filter(|(_, cost)| *cost != (0, 0))
            .collect::<BTreeMap<_, _>>()
    });
    prop_assert_eq!(now, named(name_costs(ours)));
    prop_assert_eq!(before, named(name_costs(theirs)));

    current_rows.sort_unstable();
    let rows_now = current_rows.len();
    current_rows.dedup();
    prop_assert_eq!(current_rows.len(), rows_now, "a function has two rows");
    let (ours_named, theirs_named) = (named_functions(ours), named_functions(theirs));
    for (name, count) in &rows {
        let most = ours_named
            .get(name.as_str())
            .copied()
            .unwrap_or(0)
            .max(theirs_named.get(name.as_str()).copied().unwrap_or(0));
        prop_assert_eq!(*count, most, "rows of {}", name);
    }
    prop_assert_eq!(
        rows.len(),
        ours_named
            .keys()
            .chain(theirs_named.keys())
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    );
    Ok(())
}

#[test]
fn a_diff_pairs_a_function_with_itself_before_another_of_its_name() {
    let order: Vec<usize> = (0..NAMED.len()).collect();
    a_diff_holds_each_profiles_weight(
        (&[(vec![1], 1)], &[(vec![5], 12), (vec![1], 1)]),
        (&order, &order),
    )
    .unwrap();
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn a_diff_holds_each_profiles_weight_on_every_path(
        ours in weighted_stacks(),
        theirs in weighted_stacks(),
        (our_order, their_order) in (order(), order()),
    ) {
        a_diff_holds_each_profiles_weight((&ours, &theirs), (&our_order, &their_order))?;
    }
}

type Call = (u32, i64, i64, Option<Vec<usize>>);

fn traced(
    calls: &[Call],
    samples: &[(u32, i64, Vec<usize>)],
    zones: &[(u32, i64, i64)],
) -> Capture {
    use zerocopy::FromZeros as _;

    let mut builder = CaptureBuilder::new();
    let module = builder.module("/bin/app", None, [0; 16], 0);
    let frames: Vec<u32> = (0_u64..)
        .zip(FUNCTIONS)
        .map(|(index, name)| {
            let frame = builder.frame(FrameKind::Native, module, 0x10 * index, 0);
            builder.symbolize(frame, module, &[line(name)]);
            frame
        })
        .collect();
    let threads = [
        builder.thread(Pid::new(1), Tid::new(1), 0),
        builder.thread(Pid::new(1), Tid::new(2), 0),
    ];
    assert_eq!(threads, [0, 1], "threads are their own rows");

    let mut stack = |choices: &[usize]| {
        let stack: Vec<u32> = choices.iter().map(|&choice| frames[choice]).collect();
        builder.stack(&stack)
    };
    let call_stacks: Vec<u32> = calls
        .iter()
        .map(|(.., choices)| choices.as_deref().map_or(NONE, &mut stack))
        .collect();
    let sample_stacks: Vec<u32> = samples
        .iter()
        .map(|(_, _, choices)| stack(choices))
        .collect();

    for (&(thread, entered, exited, _), stack) in calls.iter().zip(call_stacks) {
        let mut call = crate::capture::records::Syscall::new_zeroed();
        call.thread = thread.into();
        call.entered = entered.into();
        call.exited = exited.into();
        call.stack = stack.into();
        builder.syscall(call, &[]);
    }

    for (&(thread, time, _), stack) in samples.iter().zip(sample_stacks) {
        builder.sample(thread, time, 1, stack, NONE);
    }

    let site = builder.site(
        "phase",
        "app.rs",
        1,
        module,
        microscope_probes::abi::SITE_ZONE,
    );
    for &(thread, start, end) in zones {
        builder.zone(
            thread,
            crate::capture::records::Zone {
                start: start.into(),
                end: end.into(),
                site: site.into(),
                flags: 0.into(),
                value: NONE.into(),
                reserved: 0.into(),
            },
        );
    }

    builder.finish()
}

type OfFunction<'n> = (fn(u32) -> Transform, &'n str);

fn kept_calls(capture: &Capture, query: &Query, transform: Option<OfFunction<'_>>) -> Vec<usize> {
    let keys = Keys::new(capture, Granularity::Function);
    let mut query = query.clone();
    if let Some((transform, name)) = transform {
        query.transforms = keys.named(name).into_iter().map(transform).collect();
    }

    keys.syscalls(&query)
        .iter()
        .enumerate()
        .filter_map(|(row, kept)| kept.then_some(row))
        .collect()
}

#[test]
fn a_query_keeps_the_calls_made_under_its_function_on_its_threads_and_in_its_zones() {
    let calls: [Call; 5] = [
        (0, 10, 12, Some(vec![3, 1, 0])),
        (1, 20, 22, Some(vec![3, 1, 0])),
        (0, 30, 32, Some(vec![3, 2, 0])),
        (0, 40, 41, None),
        (1, crate::capture::records::UNSEEN, 52, Some(vec![1, 0])),
    ];
    let capture = traced(&calls, &[], &[(0, 25, 35)]);
    let all = Query::default();

    assert_eq!(kept_calls(&capture, &all, None), [0, 1, 2, 3, 4]);
    let under_b = Some((Transform::Focus as fn(u32) -> Transform, "b"));
    assert_eq!(kept_calls(&capture, &all, under_b), [0, 1, 4]);
    assert_eq!(
        kept_calls(&capture, &all, Some((Transform::Drop, "b"))),
        [2, 3]
    );
    assert_eq!(
        kept_calls(&capture, &all, Some((Transform::Merge, "b"))),
        [0, 1, 2, 3, 4]
    );

    let second = Query {
        threads: Some(vec![1]),
        ..Query::default()
    };
    assert_eq!(kept_calls(&capture, &second, under_b), [1, 4]);

    let ranged = Query {
        range: Some((12, 41)),
        ..Query::default()
    };
    assert_eq!(kept_calls(&capture, &ranged, None), [0, 1, 2]);

    let zoned = Query {
        zones: Some(ZoneScope {
            sites: sites_named(&capture, "phase"),
            band: None,
            outside: false,
        }),
        ..Query::default()
    };
    assert_eq!(kept_calls(&capture, &zoned, None), [2]);
}

#[test]
fn samples_during_calls_are_those_their_threads_took_inside_them() {
    let calls: [Call; 3] = [
        (0, 10, 20, Some(vec![3])),
        (1, 10, 20, Some(vec![3])),
        (0, 50, crate::capture::records::UNSEEN, Some(vec![3])),
    ];
    let samples = [
        (0, 9, vec![0]),
        (0, 10, vec![1]),
        (0, 19, vec![2]),
        (0, 20, vec![0]),
        (1, 15, vec![4]),
        (0, 70, vec![4]),
    ];
    let capture = traced(&calls, &samples, &[]);
    let keys = Keys::new(&capture, Granularity::Function);
    let during = |rows: Vec<u32>| {
        keys.aggregate(&Query {
            during: Some(rows),
            ..Query::default()
        })
        .samples
    };

    assert_eq!(during(vec![0]), 2);
    assert_eq!(during(vec![0, 1]), 3);
    assert_eq!(during(vec![2]), 1);
    assert_eq!(during(Vec::new()), 0);
}

type Sampled = (u32, i64, Vec<usize>);

fn calls_and_samples() -> impl Strategy<Value = (Vec<Call>, Vec<Sampled>)> {
    let stack = prop::collection::vec(0..FUNCTIONS.len(), 1..4);
    let call = (
        0..2_u32,
        0..100_i64,
        0..10_i64,
        prop::option::of(stack.clone()),
    )
        .prop_map(|(thread, entered, length, stack)| (thread, entered, entered + length, stack));
    (
        prop::collection::vec(call, 0..24),
        prop::collection::vec((0..2_u32, 0..120_i64, stack), 0..24),
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// A query keeps a call of its threads that returned in its range and
    /// whose stack names its focus, or does not name what it drops.
    #[test]
    fn the_calls_a_query_keeps_are_those_its_selection_names(
        (calls, _) in calls_and_samples(),
        thread in prop::option::of(0..2_u32),
        range in prop::option::of((0..110_i64, 0..40_i64)),
        transform in prop::option::of((any::<bool>(), 0..FUNCTIONS.len())),
    ) {
        let capture = traced(&calls, &[], &[]);
        let query = Query {
            threads: thread.map(|thread| vec![thread]),
            range: range.map(|(start, length)| (start, start + length)),
            ..Query::default()
        };
        let named: Option<OfFunction<'_>> = transform.map(|(focus, function)| {
            let make: fn(u32) -> Transform = if focus { Transform::Focus } else { Transform::Drop };
            (make, FUNCTIONS[function])
        });

        let expected: Vec<usize> = calls
            .iter()
            .enumerate()
            .filter(|(_, (called, _, exited, stack))| {
                let names = |function: usize| stack.as_ref().is_some_and(|stack| stack.contains(&function));
                thread.is_none_or(|thread| thread == *called)
                    && query.range.is_none_or(|(start, end)| (start..end).contains(exited))
                    && transform.is_none_or(|(focus, function)| names(function) == focus)
            })
            .map(|(row, _)| row)
            .collect();
        prop_assert_eq!(kept_calls(&capture, &query, named), expected);
    }

    /// The samples during chosen calls are those each thread took while
    /// inside one of its own.
    #[test]
    fn the_samples_during_calls_are_those_inside_them(
        (calls, samples) in calls_and_samples(),
        chosen in prop::collection::vec(any::<bool>(), 24),
    ) {
        let capture = traced(&calls, &samples, &[]);
        let rows: Vec<u32> = (0..calls.len())
            .filter(|&row| chosen[row])
            .map(|row| u32::try_from(row).unwrap_or(u32::MAX))
            .collect();
        let expected = samples
            .iter()
            .filter(|(thread, time, _)| {
                rows.iter().any(|&row| {
                    let (called, entered, exited, _) = &calls[row as usize];
                    called == thread && (*entered..*exited).contains(time)
                })
            })
            .count() as u64;

        let keys = Keys::new(&capture, Granularity::Function);
        let kept = keys.aggregate(&Query { during: Some(rows), ..Query::default() }).samples;
        prop_assert_eq!(kept, expected);
    }
}

#[test]
fn a_kernel_lane_counts_calls_where_they_returned_and_spans_the_calls_a_thread_waited_in() {
    let calls: [Call; 6] = [
        (0, 5, 8, None),
        (0, 12, 47, None),
        (1, 95, 130, None),
        (1, 50, 52, None),
        (0, 200, 210, None),
        (1, 70, UNSEEN_TIME, None),
    ];
    let mut capture = traced(&calls, &[], &[]);
    capture.syscalls[3].outcome = crate::capture::records::CallOutcome::Failed as u8;
    capture.syscalls[5].outcome = crate::capture::records::CallOutcome::Unfinished as u8;
    let keys = Keys::new(&capture, Granularity::Function);
    let query = Query {
        range: Some((0, 100)),
        ..Query::default()
    };

    let lanes = kernel_lanes(&capture, &keys, &query, 10);
    let slices = |marked: &[(usize, u32)]| {
        let mut counts = vec![0; 10];
        for &(slice, count) in marked {
            counts[slice] = count;
        }
        counts
    };
    assert_eq!(
        lanes,
        [
            KernelLane {
                thread: 0,
                calls: slices(&[(0, 1), (4, 1)]),
                failed: slices(&[]),
                long: vec![(1, 12, 47)],
            },
            KernelLane {
                thread: 1,
                calls: slices(&[(5, 1), (7, 1)]),
                failed: slices(&[(5, 1)]),
                long: vec![(5, 70, 100), (2, 95, 100)],
            },
        ]
    );

    let one = kernel_lanes(
        &capture,
        &keys,
        &Query {
            threads: Some(vec![1]),
            ..query
        },
        10,
    );
    assert_eq!(one.iter().map(|lane| lane.thread).collect::<Vec<_>>(), [1]);
    assert!(kernel_lanes(&capture, &keys, &Query::default(), 10).is_empty());
}

const UNSEEN_TIME: i64 = crate::capture::records::UNSEEN;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Each call the query keeps that returned in the range counts once,
    /// in the slice its return falls in, and a call that spans a slice's
    /// width is drawn over its part of the range.
    #[test]
    fn a_kernel_lane_holds_each_kept_call_once(
        (calls, _) in calls_and_samples(),
        thread in prop::option::of(0..2_u32),
        (start, length) in (0..60_i64, 1..80_i64),
        buckets in 1..12_usize,
    ) {
        let capture = traced(&calls, &[], &[]);
        let keys = Keys::new(&capture, Granularity::Function);
        let end = start + length;
        let query = Query {
            threads: thread.map(|thread| vec![thread]),
            range: Some((start, end)),
            ..Query::default()
        };
        let lanes = kernel_lanes(&capture, &keys, &query, buckets);

        let mut expected: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        let width = length / i64::try_from(buckets).unwrap();
        let mut long: BTreeMap<u32, Vec<(u32, i64, i64)>> = BTreeMap::new();
        for (row, &(called, entered, exited, _)) in calls.iter().enumerate() {
            if thread.is_some_and(|thread| thread != called) {
                continue;
            }
            if (start..end).contains(&exited) {
                let slice = usize::try_from((exited - start) * i64::try_from(buckets).unwrap() / length).unwrap();
                expected.entry(called).or_insert_with(|| vec![0; buckets])[slice] += 1;
            }
            if exited - entered >= width.max(1) && entered.max(start) < exited.min(end) {
                long.entry(called).or_default().push((u32::try_from(row).unwrap(), entered.max(start), exited.min(end)));
            }
        }

        let threads: Vec<u32> = expected.keys().chain(long.keys()).copied().collect::<std::collections::BTreeSet<_>>().into_iter().collect();
        prop_assert_eq!(lanes.iter().map(|lane| lane.thread).collect::<Vec<_>>(), threads);
        for lane in &lanes {
            prop_assert_eq!(&lane.calls, &expected.get(&lane.thread).cloned().unwrap_or_else(|| vec![0; buckets]));
            prop_assert!(lane.failed.iter().all(|&failed| failed == 0));
            let mut spans = long.get(&lane.thread).cloned().unwrap_or_default();
            spans.sort_by_key(|&(row, from, _)| (from, row));
            let mut drawn = lane.long.clone();
            drawn.sort_by_key(|&(row, from, _)| (from, row));
            prop_assert_eq!(drawn, spans);
        }
    }
}

#[test]
fn calls_as_leaves_weigh_what_no_sample_of_their_thread_stands_for() {
    let calls: [Call; 3] = [
        (0, 100, 200, Some(vec![1, 0])),
        (1, 50, 90, None),
        (0, 300, UNSEEN_TIME, Some(vec![0])),
    ];
    let samples = [(0, 150, vec![1, 0]), (0, 250, vec![2, 0])];
    let mut capture = traced(&calls, &samples, &[]);
    for sample in &mut capture.samples {
        sample.weight = 30.into();
    }
    let number = capture.syscalls[0].number.get();
    capture.syscalls[1].number = (number + 1).into();
    let keys = Keys::new(&capture, Granularity::Function);
    let first = keys.name(keys.syscall(0, number));
    let second = keys.name(keys.syscall(0, number + 1));
    assert!(first.starts_with("[syscall] "), "{first}");

    let paths = |query: &Query| ending_paths(&keys.aggregate(query), &keys);
    let leaves = Query {
        calls: true,
        ..Query::default()
    };
    let path = |names: &[&str]| names.iter().map(|name| (*name).to_owned()).collect();
    assert_eq!(
        paths(&leaves),
        BTreeMap::from([
            (path(&["a", "b"]), 30),
            (path(&["a", "b", &first]), 70),
            (path(&["a", "c"]), 30),
            (path(&[&second]), 40),
        ])
    );
    assert_eq!(keys.aggregate(&leaves).samples, 2);
    assert!(
        paths(&Query::default())
            .keys()
            .all(|path| !path.iter().any(|name| name.starts_with('[')))
    );

    let focus = Query {
        transforms: vec![Transform::Focus(keys.syscall(0, number))],
        ..leaves.clone()
    };
    assert_eq!(paths(&focus), BTreeMap::from([(path(&[&first]), 70)]));
    let drop = Query {
        transforms: vec![Transform::Drop(keys.syscall(0, number + 1))],
        ..leaves.clone()
    };
    assert!(!paths(&drop).contains_key(&path(&[&second])));

    let during = Query {
        during: Some(vec![0]),
        ..leaves.clone()
    };
    assert_eq!(
        paths(&during),
        BTreeMap::from([(path(&["a", "b"]), 30), (path(&["a", "b", &first]), 70)])
    );

    let inverted = Query {
        invert: true,
        ..leaves
    };
    assert_eq!(paths(&inverted).get(&path(&[&first, "b", "a"])), Some(&70));
}
