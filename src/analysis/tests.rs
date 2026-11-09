use std::collections::BTreeMap;

use proptest::prelude::*;

use super::*;
use crate::capture::Capture;
use crate::capture::builder::{CaptureBuilder, InterpretedPlace, LineInput};
use crate::capture::records::{FrameKind, NONE, Truncation};
use crate::model::{Pid, Tid};

const FUNCTIONS: [&str; 5] = ["a", "b", "c", "d", "e"];

/// Goroutines 1 to 3 by choice, the second labeled, and this choice for
/// none.
const UNNAMED: u32 = 3;

fn task_row(choice: u32) -> u32 {
    if choice == UNNAMED { NONE } else { choice }
}

/// How a query by task names a task by its choice.
fn group_name(choice: u32) -> String {
    match choice {
        UNNAMED => "[no task known]".to_owned(),
        1 => "goroutine 2 {phase=spin}".to_owned(),
        _ => format!("goroutine {}", choice + 1),
    }
}

/// A capture of `stacks` (frame choices, innermost first, and whether a
/// truncation ends them), `samples` (thread, time, weight, stack, task
/// choice), and `waits` (time, weight, stack, task choice). Frame 5 has no
/// symbols, frames 6 and 7 inline one function into another, frame 8 is
/// `a` again in the module made again at the same path, as node's perf
/// map makes it, and frame 9 is another `a`, of another module. Each
/// frame's lines are at lines of their own.
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

/// A transform of a key, by what views show of it.
#[derive(Debug, Clone)]
enum Named {
    Focus(String),
    Merge(String),
    Drop(String),
    Collapse(String),
    Cycles,
}

/// `path` with each cycle cut out, walked from its outermost frame: a
/// function already on the path cuts it back to its earlier call.
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

/// What views show of a key: its name and its module's.
fn shown(keys: &Keys<'_>, key: u32) -> String {
    format!(
        "{} [{}]",
        keys.name(key),
        keys.module(key).unwrap_or_default()
    )
}

/// What views show of each key of a frame, innermost first, worked out
/// from the capture's tables alone: a function by its name, a line by its
/// function's name, file, and number, and code no symbols describe, or
/// any code at address granularity, by its module and address. Each shows
/// its module's file name, so that a module made again at one path is one
/// module.
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

/// The keys of a sample's path, as views show them, computed from the
/// capture's tables alone.
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

/// Stacks, samples, and waits as [`capture`] takes them.
type Stacks = Vec<(Vec<usize>, bool)>;
type Samples = Vec<(u32, i64, u64, usize, u32)>;
type Waits = Vec<(i64, u64, usize, u32)>;

/// A range, threads, transforms by kind and key, and inversion; whether
/// waits are queried, tasks by choice, and whether by task; and the
/// granularity.
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

/// How many frames [`capture`] makes.
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

/// What the oracle follows: each selected sample's or wait's task, stack,
/// and weight.
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

        // Every key a frame has, by what views show of it: one key each.
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

        // The oracle: each selected sample alone.
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

            // A node's subtree is the run of deeper nodes after it.
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

    // The sample at 10 lies outside [0, 10).
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
    builder.off_cpu(a, 5, 20, false);
    builder.off_cpu(a, 30, 4, true);
    builder.off_cpu(a, 45, 10, false);
    builder.off_cpu(b, -5, 100, false);

    let capture = builder.finish();
    let query = Query {
        range: Some((0, 40)),
        ..Query::default()
    };

    // Slices of ten from 0; b was off from before the range to after it.
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

    // Slices of an uneven range share it out without losing any.
    let uneven = Query {
        range: Some((0, 10)),
        threads: Some(vec![b]),
        ..Query::default()
    };

    let lanes = off_cpu(&capture, &uneven, 3);
    assert_eq!(lanes.len(), 1);
    assert_eq!(lanes[0].blocked.iter().sum::<u64>(), 10, "{lanes:?}");
}

/// The analysis is pure: it reads captures and nothing else, so a query
/// answers the same wherever it runs. Tests are exempt.
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

/// Each path of the tree that samples end at, outermost first, with the
/// weight that ends there.
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

    // The native stacks are one switch away.
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

/// A Python program's capture: a sample of a mixed stack, its interpreted
/// frames inside the activations of the interpreter that run them and
/// calling native code, and one of a thread that runs no Python, with the
/// frames of `Server.handle` and the native `decode` it calls.
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

/// A Python program's stacks are mixed, its interpreted frames inside the
/// activations of the interpreter that run them. The Python look shows
/// them from the outermost in, without the interpreter, and with the native
/// code they call; the native look shows only what the thread ran; and a
/// stack with no interpreted frame is the same in every look.
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

    // An interpreted frame has no address: by address, it is its line.
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

/// A frame of an anonymous function: its file, three lines into its
/// function, or none for compiled code; where its function is declared;
/// and its sample's weight.
type Anonymous<'a> = (Option<&'a str>, Option<(&'a str, u32)>, u64);

/// A capture of one sample of each frame of `places`, each of an anonymous
/// interpreted function as V8 names them, and the frames.
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

/// A function a module made again names keys once: node's perf map is
/// read again as V8 compiles more, each reading a new module over V8's code
/// at one path, and the native view once listed one JavaScript function as
/// 274 rows. A function of that name in another module stays apart.
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

/// Functions of one name, as many lint rules' `create` are, each have an
/// exact name that a focus takes and that picks out that function alone,
/// while a name no other function has stays as it is. The browser selects
/// functions by these names, and once could select neither `create`.
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

/// An anonymous JavaScript function is named for where it is declared, as
/// V8's stack traces show it, so that no row is blank, `--focus` can name
/// it, and two of them pair in a diff only if declared in one place. A
/// native function with no name is not one.
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

/// A capture of one sample of each of `stacks`, outermost first, with its
/// weight, each function a frame of its own.
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

/// Collapsing every cycle cuts each path back to a function's first call
/// whenever it is called again, however the call came round: directly, by
/// two functions calling each other, by three, and again after the path
/// left the cycle. Totals at the roots stay what they were.
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
            // a b a c a: the second a cuts back to the first, then c is
            // new, then the third a cuts c off again.
            (path(&["a"]), 8),
            (path(&["main", "visit"]), 16),
        ])
    );

    // Own cost moves to the outermost call; the roots' totals do not
    // change, though a function only a cut cycle ran loses that sample.
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

    // Inverted, each path from its innermost frame is cut the same way.
    assert_eq!(
        paths(vec![Transform::CollapseCycles], true)[&path(&["work", "c", "b", "a"])],
        4
    );

    // A collapse of one function leaves cycles through others.
    let self_key = keys.named("self")[0];
    let one = paths(vec![Transform::CollapseRecursion(self_key)], false);
    assert_eq!(one[&path(&["main", "self", "work"])], 1);
    assert_eq!(
        one[&path(&["main", "visit", "children", "visit", "children", "work"])],
        2
    );
}

/// A C++ function is named with its parameter list, which can run past
/// 200 characters, so a name without it picks the function out when it
/// is the only one so named; overloads are all found, for the caller to
/// tell apart; and a name nothing has finds the names that contain it.
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

    // A whole name still wins over another's prefix, and a prefix that
    // stops inside a name is no match.
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

/// A sample: its thread, time, weight, and task choice.
type Sample = (u32, i64, u64, u32);

/// A capture of label spans `route=vN` (thread, start, length, N), and of
/// samples of one stack, goroutine 2 labeled `phase=spin`.
fn labeled(spans: &[(u32, i64, i64, u32)], samples: &[Sample]) -> Capture {
    labeled_off(spans, samples, &[])
}

/// A span of time off a CPU: thread, start, length, and whether it was
/// preempted rather than blocked.
type Off = (u32, i64, u64, bool);

/// [`labeled`], its threads off their CPUs for `off`, which may not overlap
/// on one thread.
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
        builder.off_cpu(threads[thread as usize], time, duration, preempted);
    }

    builder.finish()
}

/// A sample carries the labels of the spans open on its thread at its
/// time, and its task's: a query of a label keeps exactly the samples that
/// carry it, and one by a key roots each at its innermost value.
fn samples_carry_the_open_labels(
    spans: &[(u32, i64, i64, u32)],
    samples: &[Sample],
    wanted: u32,
) -> Result<(), TestCaseError> {
    let capture = labeled(spans, samples);
    let keys = Keys::new(&capture, Granularity::Function);

    // The innermost route a sample carries: the latest-starting span
    // that covers it, of two that start together the shorter, and of
    // two alike the first recorded, which the capture nests inside.
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
        // A key no label has roots every sample at its absence.
        expected = expected
            .into_values()
            .map(|weight| ("[no such label]".to_owned(), weight))
            .collect();
    }

    prop_assert_eq!(roots, expected);
    Ok(())
}

/// Found by the property below: two spans alike but for their value, so
/// the first recorded is the innermost.
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
        // Each thread's spans off its CPU, one after another.
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
        let blocks = zone_blocks(&capture, range, None, fold);

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

        // Threads are told apart by row in the blocks, by choice here.
        let rows: BTreeMap<u32, (i64, i64, i64)> = drawn
            .into_iter()
            .map(|(row, totals)| (capture.threads[row as usize].tid.get() - 1, totals))
            .collect();
        prop_assert_eq!(rows, expected);
    }

    #[test]
    fn zone_blocks_draw_each_zone_once_and_fold_only_narrow_ones(
        spans in proptest::collection::vec((0..2_u32, 0..400_i64, 0..60_i64, 0..3_u32), 0..40),
        range in (0..300_i64, 1..300_i64),
        fold in 1..40_i64,
    ) {
        let capture = labeled(&spans, &[]);
        let range = (range.0, range.0 + range.1);
        let blocks = zone_blocks(&capture, range, None, fold);
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

        // The spans in the range, as recorded, each thread's choice its row.
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

        // Of two zones, the one that begins first, or of two that begin
        // together the longer, holds the other where the other begins
        // inside it, and is drawn above it.
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

/// A zone scope keeps a sample once where any zone of its site, in or out
/// of the band as asked, covers it on its own thread, however the zones
/// nest; and the stats of the same band count and rank exactly its zones.
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

/// A zone nested in another of its site adds nothing: the sample inside
/// both counts once, and a sample of the other thread at the same time
/// counts not at all.
#[test]
fn a_nested_zone_counts_its_samples_once() {
    let spans = [(0, 10, 50, 0), (0, 20, 10, 1)];
    let capture = labeled(&spans, &[(0, 25, 3, 0), (1, 25, 7, 0), (0, 70, 11, 0)]);
    let keys = Keys::new(&capture, Granularity::Function);
    let scope = |band| ZoneScope {
        sites: sites_named(&capture, "route"),
        band,
        outside: false,
    };
    let weight = |band| {
        keys.aggregate(&Query {
            zones: Some(scope(band)),
            ..Query::default()
        })
        .weight
    };

    assert_eq!(weight(None), 3);
    assert_eq!(weight(Some((10, 10))), 3, "the band holds its edges");
    assert_eq!(weight(Some((11, 49))), 0);

    let none = ZoneScope {
        sites: Vec::new(),
        band: None,
        outside: false,
    };
    assert_eq!(
        keys.aggregate(&Query {
            zones: Some(none),
            ..Query::default()
        })
        .weight,
        0
    );
}

/// A series is every site of one name, binned by time: each bin keeps its
/// first, last, lowest, and highest value, values no axis can show are
/// counted, not drawn, and the value before the range holds into it.
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

    // A value holds until the next: a range between values still shows
    // the one before it, and whether one follows.
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

/// Validation lets a plot be of no site, which is a series of no name.
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

/// The functions diffs' stacks choose from: a name and its module. Two
/// names are each two functions, of two modules, as views name them alike.
const NAMED: [(&str, &str); 6] = [
    ("a", "/bin/app"),
    ("b", "/bin/app"),
    ("c", "/bin/app"),
    ("a", "/lib/libz.so"),
    ("d", "/lib/libz.so"),
    ("b", "/lib/libz.so"),
];

/// A stack of indices into [`NAMED`], innermost first, and its weight.
type WeightedStack = (Vec<usize>, u64);

/// A capture of one thread's samples of stacks of [`NAMED`], innermost
/// first, its functions made in `order`, so that their keys are in that
/// order.
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

/// The weight of the samples whose path of names, outermost first, is
/// each path, and of those whose path begins with it.
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

/// Each name's own and total weight, summed over the functions of that
/// name: a sample is in a function's total once however often it calls
/// it.
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

/// How many functions of each name the samples call.
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

/// Every function's place, in some order.
fn order() -> impl Strategy<Value = Vec<usize>> {
    Just((0..NAMED.len()).collect::<Vec<_>>()).prop_shuffle()
}

/// Two profiles' trees merge into one whose every path holds exactly each
/// profile's own and total weight there, whichever has it, nested as trees
/// are, however each made its functions and wherever two share a name, a
/// function paired with itself where both have it; and their costs merge
/// so that each function of each profile has one row, and each name's are
/// each profile's.
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

    // Each path's own and total weight now and before, over every node
    // of that path of names.
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

    // A path the base has too, function for function, is paired with
    // it, not with another function of the same names.
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

    // A function the base calls too is paired with itself.
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

    // Every function has a row, and a row pairs at most one of each.
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

/// Found by the property below: the current profile's `b` paired with the
/// base's other `b`, which came first, rather than with itself.
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
