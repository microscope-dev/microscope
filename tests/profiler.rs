mod support;

use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use microscope::profiler::budget::Budget;
use microscope::profiler::{
    Attachment, CallNode, DEEP_STACK_COPY, GapReason, Granularity, MarkerKind, Preset,
    ProbeSelection, Profile, Query, RecordError, RecordOptions, SMALL_STACK_COPY, SampleSource,
    SampledStack, Save, StackCopy, StackEnding, StackView, Watch, record, record_attached,
    record_watched,
};
use support::Scenario;

/// The profile-stacks fixture across languages, compilers, optimization,
/// frame pointers, PIE, and linkers, and whether the build keeps frame
/// pointers.
const VARIANTS: [(&str, bool); 4] = [
    ("gcc-o0", true),
    ("gcc-o2-nofp", false),
    ("clang-o2", true),
    ("clang-o2-nofp", false),
];

/// The same chains in C++, in Rust, and linked by mold, with the names of
/// the fixture's middle and outer functions and of its main.
const OTHER_VARIANTS: [(&str, [&str; 3]); 5] = [
    ("gcc-mold", ["middle", "outer", "descend"]),
    (
        "cpp-gcc-o2-nofp",
        ["stacks::middle", "stacks::outer", "main"],
    ),
    ("cpp-clang-o0", ["stacks::middle", "stacks::outer", "main"]),
    (
        "rust-o0",
        [
            "profile_stacks::middle",
            "profile_stacks::outer",
            "profile_stacks::main",
        ],
    ),
    (
        "rust-o2-nofp",
        [
            "profile_stacks::middle",
            "profile_stacks::outer",
            "profile_stacks::main",
        ],
    ),
];

fn command(variant: &str, iterations: u64) -> Command {
    let mut command = Command::new(Scenario::fixture(&format!("profile-stacks-{variant}")));
    command
        .arg(iterations.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// Options that sample each call of the function `symbol` names. A count
/// is exact only while no sample is lost, so the rings carry nothing else.
fn calls_of(symbol: &str) -> RecordOptions {
    RecordOptions {
        source: SampleSource::Function {
            symbol: symbol.into(),
        },
        off_cpu: false,
        ..RecordOptions::default()
    }
}

/// The stack of each call of `leaf` on each thread, innermost first.
fn expected(stack: &SampledStack) -> Vec<&'static str> {
    let mut expected = vec!["leaf", "middle", "outer"];
    if stack.functions.get(3).map(String::as_str) == Some("worker") {
        expected.extend(["worker", "start_thread", "__GI___clone3"]);
    } else {
        expected.extend(["descend"; 5]);
        expected.extend([
            "main",
            "__libc_start_call_main",
            "__libc_start_main",
            "_start",
        ]);
    }

    expected
}

#[test]
fn a_function_breakpoint_records_its_exact_callers_in_every_build() {
    for (variant, frame_pointers) in VARIANTS {
        for copy in [DEEP_STACK_COPY, SMALL_STACK_COPY] {
            let options = RecordOptions {
                stack: StackCopy::Bytes(copy),
                ..calls_of("leaf")
            };

            let recording = record(command(variant, 1), &options).expect("record");
            assert!(recording.status.success());
            let stacks = recording.profile.stacks();
            assert_eq!(stacks.len(), 6, "{variant} {copy}: every call is a sample");
            for stack in &stacks {
                let expected = expected(stack);
                let context = format!("{variant} {copy}: {stack:?}");

                // A stack is never wrong: what it has is exact, and where
                // it stops short it says why.
                match &stack.ending {
                    StackEnding::Complete => assert_eq!(stack.functions, expected, "{context}"),
                    StackEnding::Truncated(reason) => {
                        assert!(
                            stack.functions.len() <= expected.len()
                                && stack.functions.iter().zip(&expected).all(|(a, b)| a == b),
                            "{context}: not a prefix"
                        );
                        assert_eq!(reason, "stack copy exhausted", "{context}");
                    }
                }

                // A deep copy unwinds everything, and a small one does too
                // where the kernel's chain continues it.
                if copy == DEEP_STACK_COPY || frame_pointers {
                    assert_eq!(stack.ending, StackEnding::Complete, "{context}");
                } else if expected.contains(&"main") {
                    // The main thread's frames outgrow a small copy, so the
                    // frame-pointer builds complete by the kernel's chain.
                    assert_ne!(stack.ending, StackEnding::Complete, "{context}");
                }
            }
        }
    }
}

#[test]
fn auto_mode_copies_a_small_stack_only_where_the_chain_carries_every_stack() {
    for (variant, bytes, why) in [
        (
            "musl",
            SMALL_STACK_COPY,
            "auto: the program keeps the frame-pointer chain where it calls",
        ),
        (
            "musl-nofp",
            DEEP_STACK_COPY,
            "auto: main calls without keeping the frame-pointer chain",
        ),
        (
            "gcc-o0",
            DEEP_STACK_COPY,
            "auto: the program loads libraries, which are known only once it runs",
        ),
    ] {
        let options = RecordOptions {
            stack: StackCopy::Auto,
            ..calls_of("leaf")
        };

        let recording = record(command(variant, 1), &options).expect("record");
        assert!(recording.status.success());
        let profile = &recording.profile;
        assert_eq!(
            profile.setting("stack bytes"),
            Some(bytes.to_string().as_str()),
            "{variant}"
        );
        assert_eq!(profile.setting("stack copy"), Some(why), "{variant}");

        // The copy auto mode chose unwinds what the deepest does.
        let deep = RecordOptions {
            stack: StackCopy::Bytes(DEEP_STACK_COPY),
            ..options
        };

        let deep = record(command(variant, 1), &deep).expect("record");
        let shapes = |stacks: Vec<SampledStack>| {
            let mut shapes: Vec<_> = stacks
                .into_iter()
                .map(|stack| (stack.functions, stack.ending))
                .collect();
            shapes.sort();
            shapes
        };

        let stacks = shapes(profile.stacks());
        assert_eq!(stacks.len(), 6, "{variant}: every call is a sample");
        assert_eq!(stacks, shapes(deep.profile.stacks()), "{variant}");
        for (functions, _) in &stacks {
            assert_eq!(functions[..3], ["leaf", "middle", "outer"], "{variant}");
        }
    }
}

/// Whether `functions`, innermost first, is a call chain of the fixture's
/// own code that its source allows: a suffix of a leaf call's stack.
fn fixture_chain(functions: &[String]) -> Option<bool> {
    let ours = ["leaf", "middle", "outer", "descend", "worker", "main"];
    if !ours.contains(&functions.first()?.as_str()) {
        return None;
    }

    let names: Vec<&str> = functions.iter().map(String::as_str).collect();
    let main = [
        "main",
        "__libc_start_call_main",
        "__libc_start_main",
        "_start",
    ];
    let worker = ["worker", "start_thread", "__GI___clone3"];

    // A sample in the worker itself, around its call of outer.
    if names.first() == Some(&"worker") {
        return Some(worker.starts_with(&names));
    }

    let mut rest = &names[..];
    for prefix in ["leaf", "middle"] {
        if rest.first() == Some(&prefix) {
            rest = &rest[1..];
        }
    }

    if rest.first() == Some(&"outer") {
        rest = &rest[1..];
        if rest.first() == Some(&"worker") {
            return Some(worker.starts_with(rest));
        }

        // From outer, every frame of the descent.
        if !rest.starts_with(&["descend"; 5]) {
            return Some(false);
        }
    }

    let descents = rest.iter().take_while(|name| **name == "descend").count();
    Some(descents <= 5 && main.starts_with(&rest[descents..]))
}

/// A sample can land in the worker's own instructions, around its call of
/// outer, as one can in main's. A stress run recorded this one.
#[test]
fn a_sample_in_the_worker_itself_is_one_of_the_fixtures_chains() {
    let chain = |names: &[&str]| {
        fixture_chain(
            &names
                .iter()
                .map(|&name| name.to_owned())
                .collect::<Vec<_>>(),
        )
    };

    assert_eq!(
        chain(&["worker", "start_thread", "__GI___clone3"]),
        Some(true)
    );
    assert_eq!(
        chain(&[
            "leaf",
            "middle",
            "outer",
            "worker",
            "start_thread",
            "__GI___clone3"
        ]),
        Some(true)
    );

    // The worker calls only outer.
    assert_eq!(
        chain(&["leaf", "worker", "start_thread", "__GI___clone3"]),
        Some(false)
    );
    assert_eq!(chain(&["worker", "main"]), Some(false));
}

#[test]
fn function_breakpoints_name_every_language_and_layout_exactly() {
    for (variant, [middle, outer, main]) in OTHER_VARIANTS {
        let options = calls_of("leaf");
        let recording = record(command(variant, 1), &options).expect("record");
        assert!(recording.status.success());
        let stacks = recording.profile.stacks();
        assert_eq!(stacks.len(), 6, "{variant}: every call is a sample");
        let mut main_thread = 0;
        for stack in &stacks {
            let context = format!("{variant}: {stack:?}");

            // Code inlined into leaf's first instruction is shown inside it.
            let leaf = stack.functions.iter().position(|name| name == "leaf");
            let own = &stack.functions[leaf.expect(&context)..];
            assert_eq!(own[..3], ["leaf", middle, outer], "{context}");
            main_thread += usize::from(own[3..].iter().any(|name| name == main));
            assert_eq!(stack.ending, StackEnding::Complete, "{context}");
            let outermost = stack.functions.last().map(String::as_str);
            assert!(
                matches!(outermost, Some("_start" | "__GI___clone3")),
                "{context}"
            );
        }

        assert_eq!(
            main_thread, 3,
            "{variant}: the main thread's calls name {main}"
        );
    }
}

/// Each path of a call tree that samples end at, outermost first, with
/// the weight that ends there.
fn ending_paths(tree: &[CallNode]) -> Vec<(Vec<&str>, u64)> {
    (0..tree.len())
        .filter(|node| tree[*node].own > 0)
        .map(|node| {
            let mut path = Vec::new();
            let mut at = Some(node);
            while let Some(node) = at {
                path.push(tree[node].name.as_str());
                at = tree[node].parent;
            }

            path.reverse();
            (path, tree[node].own)
        })
        .collect()
}

#[test]
fn an_async_tasks_samples_show_its_own_stack_of_awaits() {
    for variant in ["o0", "o3"] {
        let mut command = Command::new(Scenario::fixture(&format!("tokio-profile-{variant}")));
        command.stdout(Stdio::null());

        // Rings too small for more than a few samples lose most calls.
        let options = RecordOptions {
            ring_pages: 1,
            ..calls_of("profile::work")
        };

        let recording = record(command, &options).expect("record");
        assert!(recording.status.success());
        let profile = &recording.profile;

        // Four tasks call work five times each, and every call a full ring
        // drops is counted.
        let recorded = profile.total_weight();
        assert!(recorded > 0, "{variant}");
        assert_eq!(recorded + profile.lost_samples(), 20, "{variant}");

        let logical_tree = profile.call_tree(StackView::Logical);
        let logical = ending_paths(&logical_tree);
        assert_eq!(logical.len(), 1, "{variant}: {logical:?}");
        let (path, weight) = &logical[0];
        assert_eq!(*weight, recorded);
        let context = format!("{variant}: {path:?}");
        assert!(
            path[0]
                .starts_with("<tokio::runtime::task::harness::Harness<profile::task::{closure#0}"),
            "{context}"
        );
        let work = path[1..].iter().position(|name| *name == "profile::work");
        let (own, inlined) = path[1..].split_at(work.expect(&context));
        assert_eq!(
            own,
            [
                "[task] profile::task::{closure#0}",
                "profile::task::{closure#0}",
                "profile::outer::{closure#0}",
                "profile::inner::{closure#0}",
            ],
            "{context}"
        );

        // Code inlined into work's first instruction is shown inside it.
        assert!(
            inlined[1..]
                .iter()
                .all(|name| name.starts_with("core::hint::")),
            "{context}"
        );

        // The threads' own frames are one switch away.
        let native_tree = profile.call_tree(StackView::Native);
        let native = ending_paths(&native_tree);
        assert!(
            native.iter().all(|(path, _)| path[0] == "__GI___clone3"),
            "{variant}: {native:?}"
        );
    }
}

#[test]
fn a_watched_recording_publishes_as_it_goes_and_stops_when_asked() {
    use std::os::unix::process::ExitStatusExt as _;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let stop = Arc::new(AtomicBool::new(false));
    let (published, snapshots) = std::sync::mpsc::channel();
    let recording = std::thread::spawn({
        let stop = Arc::clone(&stop);
        move || {
            let mut command = Command::new(Scenario::fixture("spin"));
            command.stdout(Stdio::null());
            record_watched(
                command,
                &RecordOptions::default(),
                &mut Watch {
                    every: std::time::Duration::ZERO,
                    publish: &mut |profile| {
                        let pid = profile.setting("pid").map(str::to_owned);
                        let _ = published.send((profile.samples(), pid));
                    },
                    stop: &stop,
                    save: None,
                },
            )
        }
    });

    // The first profile is published as the program starts, empty.
    let deadline = std::time::Duration::from_secs(10);
    let (first, pid) = snapshots.recv_timeout(deadline).expect("the first profile");
    assert_eq!(first, 0);
    let pid = pid.expect("the program's pid");
    let mut seen = 0;
    while seen == 0 {
        seen = snapshots
            .recv_timeout(deadline)
            .expect("a profile with samples")
            .0;
    }

    stop.store(true, Ordering::Relaxed);
    let recording = recording.join().expect("the recording").expect("record");
    assert_eq!(recording.status.signal(), Some(9), "{:?}", recording.status);
    assert!(recording.profile.samples() >= seen);
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "the program is gone"
    );
}

fn go_command(variant: &str, rounds: u32) -> Command {
    let mut command = Command::new(Scenario::fixture(&format!("profile-go-{variant}")));
    command
        .arg(rounds.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

#[test]
fn a_goroutines_calls_name_its_exact_stack_in_every_build() {
    for variant in ["o0", "o2"] {
        // Go keeps the frame-pointer chain, so a small copy unwinds it, and
        // a burst of samples fits however small the rings must be.
        let options = RecordOptions {
            stack: StackCopy::Bytes(SMALL_STACK_COPY),
            ..calls_of("main.work")
        };

        let recording = record(go_command(variant, 5), &options).expect("record");
        assert!(recording.status.success());
        let profile = &recording.profile;
        let stacks = profile.stacks();

        // Four workers call work five times each.
        assert_eq!(
            stacks.len(),
            20,
            "{variant}: lost {}, counted {}, repeats {}",
            profile.lost_samples(),
            profile.counted(),
            profile.repeats()
        );

        // Every execution of the instruction is a sample or a repeat.
        assert_eq!(profile.counted(), 20 + profile.repeats(), "{variant}");
        for stack in &stacks {
            assert_eq!(
                stack.functions,
                [
                    "main.work",
                    "main.outer",
                    "main.worker",
                    "main.main.gowrap1",
                    "runtime.goexit"
                ],
                "{variant}"
            );
            assert_eq!(stack.ending, StackEnding::Complete, "{variant}");
        }
    }
}

#[test]
fn a_stack_on_the_system_stack_goes_on_into_the_goroutine_that_switched() {
    for variant in ["o0", "o2"] {
        // Every collection stops the world from the system stack.
        let options = calls_of("runtime.stopTheWorldWithSema");
        let recording = record(go_command(variant, 32), &options).expect("record");
        assert!(recording.status.success());
        let mut ours = 0;
        for stack in recording.profile.stacks() {
            let context = format!("{variant}: {stack:?}");
            let names = &stack.functions;
            let switch = names.iter().position(|name| name == "runtime.systemstack");
            let switch = switch.expect(&context);
            if switch + 1 == names.len() {
                // A switch the chain does not cross says so.
                assert_eq!(
                    stack.ending,
                    StackEnding::Truncated("frames before a stack switch not captured".into()),
                    "{context}"
                );
                continue;
            }

            // The closure the system stack ran is the switching function's.
            let (closure, switcher) = (&names[switch - 1], &names[switch + 1]);
            assert!(
                closure.starts_with(&format!("{switcher}.func")),
                "{context}"
            );
            assert_eq!(stack.ending, StackEnding::Complete, "{context}");
            assert_eq!(
                names.last().map(String::as_str),
                Some("runtime.goexit"),
                "{context}"
            );

            // A worker's collection begins on its own goroutine.
            if switcher == "runtime.gcStart" && names.iter().any(|name| name == "main.worker") {
                assert_eq!(
                    names[names.len() - 4..],
                    [
                        "main.outer",
                        "main.worker",
                        "main.main.gowrap1",
                        "runtime.goexit"
                    ],
                    "{context}"
                );
                ours += 1;
            }
        }

        // The workers collect at least once.
        assert!(ours > 0, "{variant}");
    }
}

/// Each interval a thread spends off its CPU runs from leaving it to
/// running again: a sleep blocks, sharing a CPU with another spinner
/// preempts, and no sample of the thread falls in between.
#[test]
fn a_threads_time_off_its_cpu_is_each_sleep_and_each_preemption() {
    let mut command = Command::new(Scenario::fixture("profile-offcpu"));
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // Small samples, so that a ring holds the whole run though microscope,
    // under load, reads none of it: a full ring drops the switches that
    // say when a thread came back.
    let options = RecordOptions {
        stack: StackCopy::Bytes(SMALL_STACK_COPY),
        ..RecordOptions::default()
    };

    let recording = record(command, &options).expect("record");
    assert!(recording.status.success(), "{}", recording.status);
    let profile = &recording.profile;
    assert_eq!(profile.lost_samples(), 0);
    let named = |name: &str| -> Vec<u32> {
        profile
            .threads()
            .iter()
            .filter(|thread| thread.name.as_deref() == Some(name))
            .map(|thread| thread.tid)
            .collect()
    };

    let (sleeper, spinners) = (named("sleeper"), named("spinner"));
    assert_eq!(
        (sleeper.len(), spinners.len()),
        (1, 2),
        "{:?}",
        profile.threads()
    );
    let intervals = profile.off_cpu();

    // A nap's timer starts just before the thread leaves its CPU.
    let naps = intervals
        .iter()
        .filter(|interval| {
            interval.tid == sleeper[0] && !interval.preempted && interval.duration >= 19_000_000
        })
        .count();
    assert!(naps >= 5, "{naps} naps of the sleeper's 5: {intervals:?}");
    for spinner in spinners {
        assert!(
            intervals
                .iter()
                .any(|interval| interval.tid == spinner && interval.preempted),
            "spinner {spinner} was never preempted: {intervals:?}"
        );
    }

    let stacks = profile.stacks();
    for interval in &intervals {
        let end = interval.time + i64::try_from(interval.duration).expect("short");
        let inside = stacks.iter().find(|stack| {
            stack.tid == interval.tid && stack.time > interval.time && stack.time < end
        });
        assert!(
            inside.is_none(),
            "{inside:?} was sampled during {interval:?}"
        );
    }

    for pair in intervals.windows(2) {
        if pair[0].tid == pair[1].tid {
            let end = pair[0].time + i64::try_from(pair[0].duration).expect("short");
            assert!(end <= pair[1].time, "{pair:?} overlap");
        }
    }
}

/// A recording over its CPU budget samples less of each period and pauses
/// every event between: no sample falls in a pause, no off-CPU interval
/// reaches past the start of one, and each change of the share is marked.
#[test]
fn a_recording_over_its_budget_pauses_between_samples_and_says_so() {
    let (reader, mut writer) = std::io::pipe().expect("pipe");
    let mut command = Command::new(Scenario::fixture("profile-phases"));
    command
        .stdin(reader)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let options = RecordOptions {
        budget: Some(Budget {
            cpu: 0.001,
            period: Duration::from_millis(50),
        }),
        ..RecordOptions::preset(Preset::Maximum)
    };

    let stop = AtomicBool::new(false);
    let mut released = false;

    // The program spins until the budget has changed what is sampled.
    let recording = record_watched(
        command,
        &options,
        &mut Watch {
            every: Duration::from_millis(20),
            publish: &mut |profile| {
                if !released && !profile.budget_changes().is_empty() && !profile.gaps().is_empty() {
                    writer.write_all(b"x").expect("release the program");
                    released = true;
                }
            },
            stop: &stop,
            save: None,
        },
    )
    .expect("record");
    assert!(recording.status.success(), "{}", recording.status);
    let profile = &recording.profile;
    let (gaps, changes) = (profile.gaps(), profile.budget_changes());
    assert!(!gaps.is_empty() && !changes.is_empty(), "{changes:?}");
    assert!(profile.recorder_cpu().0 > 0);

    // A recording that ends paused ends where the pause does.
    let (start, end) = profile.span();
    assert!(
        gaps.iter().all(|(from, to)| start <= *from && *to <= end),
        "{gaps:?} beyond {start}..{end}"
    );
    let inside = |time: i64| gaps.iter().any(|(start, end)| *start < time && time < *end);
    let sampled = profile.stacks();
    assert!(!sampled.is_empty());
    if let Some(stack) = sampled.iter().find(|stack| inside(stack.time)) {
        panic!("{stack:?} was sampled in a gap: {gaps:?}");
    }

    for interval in profile.off_cpu() {
        let back = interval.time + i64::try_from(interval.duration).expect("short");
        assert!(
            !gaps
                .iter()
                .any(|(start, _)| interval.time < *start && *start < back),
            "{interval:?} reaches past the start of a gap: {gaps:?}"
        );
    }
}

/// Every function on any sample's stack.
fn on_stacks(profile: &Profile) -> std::collections::BTreeSet<String> {
    profile
        .stacks()
        .into_iter()
        .flat_map(|stack| stack.functions)
        .collect()
}

/// A rolling window keeps only its last span. A trigger saves the window
/// as it is while the recording goes on, and the recording ends with the
/// window it reached, counting only what that holds.
#[test]
fn a_rolling_window_saves_on_a_trigger_and_keeps_only_its_last_span() {
    let (reader, mut writer) = std::io::pipe().expect("pipe");
    let mut command = Command::new(Scenario::fixture("profile-phases"));
    command
        .stdin(reader)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let window = Duration::from_millis(100);
    let options = RecordOptions {
        window: Some(window),
        ..RecordOptions::default()
    };

    let (trigger, stop) = (AtomicBool::new(false), AtomicBool::new(false));
    let mut saved = Vec::new();
    let recording = record_watched(
        command,
        &options,
        &mut Watch {
            every: Duration::from_millis(20),
            publish: &mut |profile| {
                if on_stacks(&profile).contains("first") {
                    trigger.store(true, Ordering::Relaxed);
                }
            },
            stop: &stop,
            save: Some(Save {
                trigger: &trigger,
                to: &mut |profile| {
                    // The second phase begins only once the first is saved.
                    if saved.is_empty() {
                        writer.write_all(b"x").expect("release the program");
                    }

                    saved.push(profile);
                },
            }),
        },
    )
    .expect("record");
    assert!(recording.status.success(), "{}", recording.status);
    let (during, after) = (on_stacks(&saved[0]), on_stacks(&recording.profile));
    assert!(
        during.contains("first") && !during.contains("second"),
        "{during:?}"
    );
    assert!(
        after.contains("second") && !after.contains("first"),
        "{after:?}"
    );
    let profile = &recording.profile;
    let (start, end) = profile.span();
    let span = u64::try_from(end - start).expect("ordered");
    assert!(span >= u64::try_from(window.as_nanos()).expect("short"));
    assert!(profile.stacks().iter().all(|stack| stack.time >= start));

    // One thread ran, so the window cannot count more than its own span.
    assert!(
        profile.counted() > 0 && profile.counted() <= span + 50_000_000,
        "counted {} in {span}",
        profile.counted()
    );
}

/// A recording cut into segments tiles its time with them: each holds
/// only its own span's samples, time off a CPU, and counts, an interval
/// that spans a cut counting in each segment for its part there, and the
/// directory they are written to reads as all of them at once.
#[test]
fn segments_tile_a_recording_and_read_back_as_one() {
    let (reader, mut writer) = std::io::pipe().expect("pipe");
    let mut command = Command::new(Scenario::fixture("profile-phases"));
    command
        .stdin(reader)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let options = RecordOptions {
        segment: Some(Duration::from_millis(40)),
        ..RecordOptions::default()
    };

    let (trigger, stop) = (AtomicBool::new(false), AtomicBool::new(false));
    let mut segments: Vec<Profile> = Vec::new();

    // The first segment that saw the helper thread wait to be released.
    let mut blocked = None;
    let recording = record_watched(
        command,
        &options,
        &mut Watch {
            every: Duration::MAX,
            publish: &mut |_| {},
            stop: &stop,
            save: Some(Save {
                trigger: &trigger,
                to: &mut |profile| {
                    // Once the helper thread waits, three cuts cross its
                    // wait before it is released.
                    if blocked.is_none() && !profile.off_cpu().is_empty() {
                        blocked = Some(segments.len());
                    }

                    if blocked.is_some_and(|first| segments.len() == first + 3) {
                        writer.write_all(b"x").expect("release the program");
                    }

                    segments.push(profile);
                },
            }),
        },
    )
    .expect("record");
    assert!(recording.status.success(), "{}", recording.status);
    segments.push(recording.profile);
    assert!(segments.len() > 4, "{}", segments.len());
    let scratch = support::ScratchDir::new("profiler-segments");
    for (number, segment) in segments.iter().enumerate() {
        let (start, end) = segment.span();
        assert!(start <= end, "segment {number}: {start} {end}");
        if let Some(next) = segments.get(number + 1) {
            assert_eq!(
                end,
                next.span().0,
                "segment {number} ends where the next starts"
            );
        }

        // Each holds what happened from its start up to the next's.
        let last = number + 1 == segments.len();
        let within = |time: i64| start <= time && (time < end || last && time == end);
        assert!(segment.stacks().iter().all(|stack| within(stack.time)));
        assert!(segment.off_cpu().iter().all(|interval| {
            within(interval.time)
                && interval.time + i64::try_from(interval.duration).expect("short") <= end
        }));

        // microscope records on one thread, which runs no longer than a span,
        // give or take where the cut fell between drains; the first also
        // holds what came before the program's first record.
        let span = u64::try_from(end - start).expect("ordered");
        let (recorder, _) = segment.recorder_cpu();
        assert!(
            number == 0 || recorder <= span + 20_000_000,
            "segment {number} took {recorder} ns of microscope's CPU time in {span}"
        );
        segment
            .write_segment(scratch.path(), Duration::from_hours(1))
            .expect("write the segment");
    }

    // The helper thread's wait is in each segment it spanned.
    let first = blocked.expect("a segment saw the helper thread wait");
    assert!(
        segments[first..=first + 3]
            .iter()
            .all(|segment| !segment.off_cpu().is_empty())
    );
    let all = Profile::read(scratch.path())
        .expect("read the directory")
        .profile;
    let sum = |measure: fn(&Profile) -> u64| segments.iter().map(measure).sum::<u64>();
    assert_eq!(
        all.samples() as u64,
        sum(|profile| profile.samples() as u64)
    );
    assert_eq!(all.counted(), sum(Profile::counted));
    let off_cpu = |profile: &Profile| {
        profile
            .off_cpu()
            .iter()
            .map(|interval| interval.duration)
            .sum::<u64>()
    };

    assert_eq!(off_cpu(&all), sum(off_cpu));
    assert_eq!(
        all.span(),
        (segments[0].span().0, segments[segments.len() - 1].span().1)
    );
    let functions = on_stacks(&all);
    assert!(
        functions.contains("first") && functions.contains("second"),
        "{functions:?}"
    );
}

/// Writing a segment deletes each segment that ended longer ago than is
/// kept, and nothing else in the directory.
#[test]
fn a_segment_deletes_only_segments_older_than_kept() {
    let scratch = support::ScratchDir::new("profiler-keep");
    let old = scratch.path().join("0000000000000001.usprof");
    let other = scratch.path().join("notes.txt");
    std::fs::write(&old, b"").expect("an old segment");
    std::fs::write(&other, b"").expect("another file");
    let profile = Profile::import(b"main;work 3\n")
        .expect("a profile")
        .profile;
    let written = profile
        .write_segment(scratch.path(), Duration::from_hours(1))
        .expect("write the segment");
    assert!(!old.exists() && other.exists() && written.exists());
    let read = Profile::read(scratch.path())
        .expect("read the directory")
        .profile;
    assert_eq!(read.samples(), profile.samples());
}

#[test]
fn a_profile_says_how_much_cpu_time_its_samples_cover() {
    // At 49 samples a second, a thread's time on each CPU is sampled
    // only once a whole period of it has passed there.
    let options = RecordOptions::preset(Preset::Production);
    let recording = record(go_command("o2", 40), &options).expect("record");
    assert!(recording.status.success());
    let profile = &recording.profile;
    assert!(profile.counted() > 0);
    assert!(
        profile.total_weight() <= profile.counted(),
        "{:?}",
        profile.settings()
    );
}

#[test]
fn a_go_programs_complete_stacks_all_end_where_stacks_begin() {
    let options = RecordOptions::preset(Preset::Maximum);
    let recording = record(go_command("o2", 40), &options).expect("record");
    assert!(recording.status.success());
    for stack in recording.profile.stacks() {
        if stack.ending == StackEnding::Complete {
            let outermost = stack.functions.last().map(String::as_str);
            assert!(
                matches!(
                    outermost,
                    Some("runtime.goexit" | "runtime.mstart" | "runtime.rt0_go")
                ),
                "{stack:?}"
            );
        }
    }
}

#[test]
fn a_signal_handlers_stack_goes_on_through_what_the_signal_interrupted() {
    for variant in ["gcc-o0", "clang-o2-nofp"] {
        let mut command = Command::new(Scenario::fixture(&format!("profile-signals-{variant}")));
        command.arg("3").stdout(Stdio::null());
        let recording = record(command, &calls_of("on_signal")).expect("record");
        assert!(recording.status.success(), "{variant}");
        let stacks = recording.profile.stacks();
        assert_eq!(stacks.len(), 3, "{variant}");
        for stack in &stacks {
            assert_eq!(
                stack.functions,
                [
                    "on_signal",
                    "__restore_rt",
                    "__pthread_kill_implementation",
                    "raise",
                    "interrupted",
                    "main",
                    "__libc_start_call_main",
                    "__libc_start_main",
                    "_start"
                ],
                "{variant}"
            );
            assert_eq!(stack.ending, StackEnding::Complete, "{variant}");
        }
    }
}

/// A stack keeps at most its frame limit, the limit falling just past the
/// signal trampoline that leads to what the signal interrupted.
#[test]
fn a_signal_handlers_stack_keeps_to_its_frame_limit() {
    for variant in ["gcc-o0", "clang-o2-nofp"] {
        let mut command = Command::new(Scenario::fixture(&format!("profile-signals-{variant}")));
        command.arg("3").stdout(Stdio::null());
        let options = RecordOptions {
            max_frames: 2,
            ..calls_of("on_signal")
        };

        let recording = record(command, &options).expect("record");
        assert!(recording.status.success(), "{variant}");
        let stacks = recording.profile.stacks();
        assert_eq!(stacks.len(), 3, "{variant}");
        for stack in &stacks {
            assert_eq!(stack.functions, ["on_signal", "__restore_rt"], "{variant}");
        }
    }
}

#[test]
fn a_stack_through_the_vdso_goes_on_to_its_callers() {
    let mut command = Command::new(Scenario::fixture("profile-vdso"));
    command.arg("2000000");
    let recording = record(command, &RecordOptions::preset(Preset::Maximum)).expect("record");
    assert!(recording.status.success());
    let stacks = recording.profile.stacks();
    let mut through = 0;
    for stack in &stacks {
        let context = format!("{stack:?}");
        assert_ne!(
            stack.ending,
            StackEnding::Truncated("unmapped return address".into()),
            "{context}"
        );

        // The vDSO exports only its entry points, so its own functions are
        // known by address.
        if stack.functions[0].starts_with("[vdso]+") {
            let main = [
                "read_clock",
                "main",
                "__libc_start_call_main",
                "__libc_start_main",
                "_start",
            ];
            let tail = &stack.functions[stack.functions.len().saturating_sub(main.len())..];
            assert_eq!(tail, main, "{context}");
            assert_eq!(stack.ending, StackEnding::Complete, "{context}");
            through += 1;
        }
    }

    // Reading the clock is all the program does.
    assert!(
        through > 0,
        "{} samples, {} lost",
        stacks.len(),
        recording.profile.lost_samples()
    );
}

/// Attaches to `process`, then, once the events are open, releases it, or
/// when `stopping`, stops the recording, which records until it ends.
fn attach_and_release(
    process: &mut support::ExternalProcess,
    options: &RecordOptions,
    stopping: bool,
) -> Result<Attachment, RecordError> {
    let pid = u32::try_from(process.process_id().get()).expect("a pid");
    let stop = std::sync::atomic::AtomicBool::new(false);
    let (attached, opened) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let recording = scope.spawn(|| {
            // Owned here, so that a failed attach closes the channel.
            let attached = attached;
            let mut publish = |_| {
                let _ = attached.send(());
            };

            let mut watch = Watch {
                every: std::time::Duration::MAX,
                publish: &mut publish,
                stop: &stop,
                save: None,
            };

            record_attached(pid, options, &mut watch)
        });

        // The first profile is published once the events are open.
        if opened.recv().is_ok() {
            if stopping {
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
            } else {
                process.release();
            }
        }

        recording.join().expect("the recording")
    })
}

#[test]
fn an_attach_follows_threads_that_began_before_it_and_after() {
    let scratch = support::ScratchDir::new("profiler-attach");
    for (variant, deleted) in [("gcc-o0", false), ("clang-o2-nofp", true)] {
        let mut program = Scenario::fixture(&format!("profile-attach-{variant}"));
        if deleted {
            // A program deleted since it started is read through it.
            let copy = scratch.path().join(variant);
            std::fs::copy(&program, &copy).expect("copy the program");
            program = copy;
        }

        let mut process = support::ExternalProcess::spawn(&program);
        if deleted {
            std::fs::remove_file(&program).expect("delete the program");
        }

        let attachment =
            attach_and_release(&mut process, &calls_of("leaf"), false).expect("attach");
        assert!(attachment.exited, "{variant}");
        assert!(process.wait().success(), "{variant}");
        let mut stacks: Vec<Vec<String>> = attachment
            .profile
            .stacks()
            .into_iter()
            .map(|stack| {
                assert_eq!(stack.ending, StackEnding::Complete, "{variant}: {stack:?}");
                stack.functions
            })
            .collect();
        stacks.sort();
        let main = [
            "leaf",
            "middle",
            "outer",
            "main",
            "__libc_start_call_main",
            "__libc_start_main",
            "_start",
        ];
        let worker = [
            "leaf",
            "middle",
            "outer",
            "worker",
            "start_thread",
            "__GI___clone3",
        ];

        // Three calls on main and on each worker, the one there before the
        // attach and the one started after.
        let mut expected: Vec<Vec<String>> = std::iter::repeat_n(&main[..], 3)
            .chain(std::iter::repeat_n(&worker[..], 6))
            .map(|stack| stack.iter().map(|name| (*name).to_owned()).collect())
            .collect();
        expected.sort();
        assert_eq!(stacks, expected, "{variant}");
    }
}

/// A program in a mount namespace of its own, as in a container, whose C
/// library there is another file than the host's at the same path, has
/// that library read through its root: its frames are named and unwound.
#[test]
fn an_attach_reads_a_contained_programs_library_through_its_root() {
    let scratch = support::ScratchDir::new("profiler-container");
    let copy = scratch.path().join("libc.so.6");
    let mut process = support::ExternalProcess::spawn_command(
        Command::new("unshare")
            .args(["--user", "--map-root-user", "--mount", "sh", "-c"])
            .arg(concat!(
                r#"library=$(ldd "$2" | sed -n 's/.*libc\.so\.6 => \([^ ]*\).*/\1/p') && "#,
                r#"cp "$library" "$1" && mount --bind "$1" "$library" && exec "$2""#,
            ))
            .arg("sh")
            .arg(&copy)
            .arg(Scenario::fixture("profile-attach-gcc-o0")),
    );
    let maps = std::fs::read_to_string(format!("/proc/{}/maps", process.process_id().get()))
        .expect("the program's maps");
    let mapped = maps
        .lines()
        .find(|line| line.ends_with("/libc.so.6"))
        .expect("the program maps a C library")
        .split_whitespace()
        .collect::<Vec<_>>();
    let host = std::fs::metadata(mapped[5]).expect("the host's file at that path");
    assert_ne!(
        std::os::unix::fs::MetadataExt::ino(&host).to_string(),
        mapped[4],
        "the program maps the host's library"
    );
    let attachment = attach_and_release(&mut process, &calls_of("leaf"), false).expect("attach");
    assert!(process.wait().success());
    let mut stacks: Vec<Vec<String>> = attachment
        .profile
        .stacks()
        .into_iter()
        .map(|stack| {
            assert_eq!(stack.ending, StackEnding::Complete, "{stack:?}");
            stack.functions
        })
        .collect();
    stacks.sort();
    stacks.dedup();
    let named =
        |names: &[&str]| -> Vec<String> { names.iter().map(|name| (*name).to_owned()).collect() };
    assert_eq!(
        stacks,
        [
            named(&[
                "leaf",
                "middle",
                "outer",
                "main",
                "__libc_start_call_main",
                "__libc_start_main",
                "_start"
            ]),
            named(&[
                "leaf",
                "middle",
                "outer",
                "worker",
                "start_thread",
                "__GI___clone3"
            ]),
        ]
    );
}

#[test]
fn an_attach_past_its_event_budget_is_refused_with_the_count() {
    let mut process = support::ExternalProcess::spawn(&Scenario::fixture("profile-attach-gcc-o0"));
    let options = RecordOptions {
        max_events: 1,
        ..RecordOptions::default()
    };

    let error = attach_and_release(&mut process, &options, false).expect_err("over budget");
    let message = error.to_string();
    assert!(message.contains("more than the budget of 1"), "{message}");

    // The program goes on as if nothing had looked at it.
    process.release();
    assert!(process.wait().success());
}

#[test]
fn a_stopped_attach_leaves_its_program_running() {
    let mut process = support::ExternalProcess::spawn(&Scenario::fixture("profile-attach-gcc-o0"));
    let attachment = attach_and_release(&mut process, &calls_of("leaf"), true).expect("attach");
    assert!(!attachment.exited);

    // Nothing was sampled before the program was released.
    assert_eq!(attachment.profile.samples(), 0);
    process.release();
    assert!(process.wait().success());
}

#[test]
fn clock_samples_are_valid_stacks_and_survive_a_file() {
    let scratch = support::ScratchDir::new("profiler-clock");
    for (variant, frame_pointers) in VARIANTS {
        let mut options = RecordOptions::preset(Preset::Maximum);

        // Deep enough for the fixture's stacks, and small enough that a
        // loaded machine drops few samples.
        options.stack = StackCopy::Bytes(if frame_pointers { 512 } else { 4096 });
        let recording = record(command(variant, 2_000_000), &options).expect("record");
        assert!(recording.status.success());
        let profile = recording.profile;
        let stacks = profile.stacks();
        let mut ours = 0;
        for stack in &stacks {
            let context = format!("{variant}: {stack:?}");
            if let Some(valid) = fixture_chain(&stack.functions) {
                ours += 1;
                assert!(valid, "{context}");
            }

            if stack.ending == StackEnding::Complete {
                let outermost = stack.functions.last().map(String::as_str);
                assert!(
                    matches!(outermost, Some("_start" | "__GI___clone3")),
                    "{context}"
                );
            }
        }

        // A loaded machine may drop most samples, which the profile
        // counts; the property needs only some of the fixture's own.
        assert!(
            ours > 0,
            "{variant}: none of {} samples in the fixture, {} lost",
            stacks.len(),
            profile.lost_samples()
        );

        let path = scratch.path().join(format!("{variant}.usprof"));
        profile.write(&path).expect("write");
        let read = microscope::profiler::Profile::read(&path).expect("read");
        assert!(!read.truncated);
        assert_eq!(
            read.profile, profile,
            "{variant}: the file holds the whole profile"
        );
    }
}

/// A Go program's own pprof profiles read as exactly what it recorded:
/// every one of its allocations, in bytes, and its parked goroutines,
/// under the label they were started in.
#[test]
fn a_go_programs_own_profiles_read_as_it_wrote_them() {
    let scratch = support::ScratchDir::new("go-pprof");
    let allocs = scratch.path().join("allocs.pb.gz");
    let goroutines = scratch.path().join("goroutine.pb.gz");
    let status = Command::new(Scenario::fixture("pprof-files"))
        .arg(&allocs)
        .arg(&goroutines)
        .status()
        .expect("run the fixture");
    assert!(status.success());

    let allocs = Profile::read(&allocs).expect("the allocations").profile;
    assert_eq!(allocs.setting("weight"), Some("bytes"));
    assert!(!allocs.timed());
    let top = allocs.top(StackView::Native);
    let allocate = top
        .iter()
        .find(|cost| cost.name == "main.allocate")
        .unwrap_or_else(|| panic!("main.allocate in {top:?}"));
    assert_eq!(allocate.own, 64_000);

    let goroutines = Profile::read(&goroutines).expect("the goroutines").profile;
    assert_eq!(goroutines.setting("weight"), Some("goroutine"));
    let keys = goroutines.keys(Granularity::Function);
    let parked = keys.aggregate(&Query {
        labels: vec![("worker".to_owned(), "parked".to_owned())],
        ..Query::default()
    });
    assert_eq!(parked.weight, 3);
    let park = goroutines
        .top(StackView::Native)
        .into_iter()
        .find(|cost| cost.name == "main.park")
        .expect("main.park");
    assert_eq!(park.total, 3);
}

/// A function's samples land on its lines and its instructions: each call
/// of leaf on leaf's first instruction and line, and on the calls in
/// middle that made them, which name it; a file changed since its
/// recording shows no code.
#[test]
fn a_functions_samples_land_on_its_lines_and_its_instructions() {
    let scratch = support::ScratchDir::new("annotate");
    let program = scratch.path().join("stacks");
    std::fs::copy(Scenario::fixture("profile-stacks-gcc-o0"), &program).expect("copy");
    let mut command = Command::new(&program);
    command.arg("1").stdout(Stdio::null());
    let profile = record(command, &calls_of("leaf")).expect("record").profile;
    let everything = Query::default();

    let lines = profile.line_costs(&everything, "leaf").lines;
    assert_eq!(
        lines.iter().map(|line| line.own).sum::<u64>(),
        6,
        "{lines:?}"
    );
    assert_eq!(
        lines.iter().filter(|line| line.own > 0).count(),
        1,
        "{lines:?}"
    );

    let code = profile
        .disassemble(&everything, "leaf")
        .expect("leaf's code");
    let [leaf] = code.blocks.as_slice() else {
        panic!("one block of leaf: {code:?}");
    };

    assert_eq!(leaf.symbol, "leaf");
    assert_eq!(leaf.instructions[0].own, 6, "{leaf:?}");
    assert!(
        leaf.instructions[1..]
            .iter()
            .all(|instruction| instruction.total == 0)
    );
    assert!(code.unplaced.is_empty());

    let middle = profile
        .disassemble(&everything, "middle")
        .expect("middle's code");
    let calls: Vec<_> = middle.blocks[0]
        .instructions
        .iter()
        .filter(|instruction| instruction.total > 0)
        .collect();
    assert!(
        calls
            .iter()
            .all(|instruction| instruction.text.starts_with("call")
                && instruction.target.as_deref() == Some("leaf")),
        "{calls:?}"
    );
    assert_eq!(
        calls
            .iter()
            .map(|instruction| instruction.total)
            .sum::<u64>(),
        6
    );

    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&program)
        .expect("open");
    file.write_all(b"\0").expect("change the program");
    let changed = profile
        .disassemble(&everything, "leaf")
        .expect_err("a changed file");
    assert!(
        changed.contains("has changed since it was recorded"),
        "{changed}"
    );
}

#[test]
fn a_program_that_cannot_start_fails_the_recording() {
    let missing = Command::new("/nonexistent/program");
    let error = record(missing, &RecordOptions::default()).expect_err("no program");
    assert!(
        matches!(&error, microscope::profiler::RecordError::Start(error) if error.kind() == std::io::ErrorKind::NotFound),
        "{error:?}"
    );
}

#[test]
fn a_forked_child_is_sampled_through_the_mappings_it_inherits() {
    let options = calls_of("shared_work");
    let mut command = Command::new(Scenario::fixture("fork"));
    command.stdout(Stdio::null());
    let recording = record(command, &options).expect("record");
    assert!(recording.status.success());
    let stacks = recording.profile.stacks();
    let pids: std::collections::BTreeSet<u32> = stacks.iter().map(|stack| stack.pid).collect();
    assert_eq!(pids.len(), 2, "the parent and its child: {stacks:?}");
    for stack in &stacks {
        assert_eq!(
            stack.functions,
            [
                "shared_work",
                "main",
                "__libc_start_call_main",
                "__libc_start_main",
                "_start"
            ],
            "{stack:?}"
        );
        assert_eq!(stack.ending, StackEnding::Complete);
    }
}

#[test]
fn a_program_killed_by_a_signal_keeps_its_profile() {
    use std::os::unix::process::ExitStatusExt;
    let options = calls_of("fault");
    let recording =
        record(Command::new(Scenario::fixture("fatal-signal")), &options).expect("record");
    assert_eq!(recording.status.signal(), Some(nix::libc::SIGSEGV));
    let stacks = recording.profile.stacks();
    assert_eq!(stacks.len(), 1);
    assert_eq!(stacks[0].functions[..2], ["fault", "main"]);
}

/// Task snapshots find each goroutine where it waits: three parked on a
/// channel receive, each weighted by the time since the snapshot before,
/// while the program goes on and no sample falls in a snapshot's pause.
/// Between snapshots that agree, each sample of the goroutine that spins
/// names it, with the labels it gave itself.
#[test]
fn task_snapshots_find_each_goroutine_where_it_waits_or_runs() {
    // The program waits until two snapshots have seen every receiver, one
    // of them after a snapshot found the spinner with its labels.
    let mut labeled = None;
    let recording = snapshot_until(
        Command::new(Scenario::fixture("profile-go-waits")),
        false,
        |profile| {
            let found = receiving(profile).len();
            if labeled.is_none() && spins_labeled(profile) {
                labeled = Some(found);
            }

            labeled.is_some_and(|seen| found >= 2.max(seen + 1))
        },
    );
    let profile = &recording;
    let (taken, paused, failed) = profile.task_snapshots();
    assert!(
        taken >= 2 && paused > 0 && failed == 0,
        "{taken} {paused} {failed}"
    );
    assert!(receiving(profile).len() >= 2, "{:#?}", profile.task_waits());
    assert_receivers_wait(profile);
    let gaps = profile.gaps();
    let inside = |time: i64| gaps.iter().any(|(start, end)| *start < time && time < *end);
    if let Some(stack) = profile.stacks().iter().find(|stack| inside(stack.time)) {
        panic!("{stack:?} was sampled in a snapshot's pause: {gaps:?}");
    }

    // The spinner spins from before the first snapshot that found every
    // receiver until after the last. A sample of its own code names it.
    // The runtime runs on its thread too: Go preempts a goroutine that has
    // run for 10 ms with a signal, whose handler, like the scheduler, runs
    // on a goroutine of its own or in assembly that keeps none. A sample of
    // that names no task, never another.
    let found = receiving(profile);
    let (first, last) = (found[0], found[found.len() - 1]);
    let spun: Vec<SampledStack> = profile
        .stacks()
        .into_iter()
        .filter(|stack| {
            first < stack.time
                && stack.time < last
                && stack
                    .functions
                    .iter()
                    .any(|function| function == "main.spin")
        })
        .collect();
    let (spinner, running) = spinner_of(&spun);

    // Go may preempt the spinner, which a snapshot then finds runnable,
    // but never receiving.
    let receivers: Vec<u64> = profile
        .task_waits()
        .iter()
        .filter(|wait| {
            wait.functions
                .iter()
                .any(|function| function == "main.receive")
        })
        .map(|wait| wait.task)
        .collect();
    assert!(!receivers.contains(&spinner), "{spinner} {receivers:?}");
    let task = profile
        .tasks()
        .into_iter()
        .find(|task| task.number == spinner)
        .expect("the spinner's row");
    assert_eq!(task.labels, [("phase".to_owned(), "spin".to_owned())]);
    assert!(profile.task_samples() >= running as u64);
}

/// The spinner's task, which every sample of its own code names, and how
/// many those are. A sample of the runtime working on its thread may name
/// no task, but never another.
fn spinner_of(spun: &[SampledStack]) -> (u64, usize) {
    let own_code = |stack: &&SampledStack| {
        stack
            .functions
            .iter()
            .take_while(|function| *function != "main.spin")
            .all(|function| !function.starts_with("runtime."))
    };

    let running: Vec<&SampledStack> = spun.iter().filter(own_code).collect();
    assert!(!running.is_empty(), "{spun:?}");
    let spinner = running[0].task.expect("the spinner's task");
    assert!(
        running.iter().all(|stack| stack.task == Some(spinner)),
        "{running:?}"
    );
    assert!(
        spun.iter()
            .all(|stack| stack.task.is_none_or(|task| task == spinner)),
        "{spun:?}"
    );
    (spinner, running.len())
}

/// The samples of the spinner a stress run once caught, one of them as Go's
/// preemption signal entered its handler on the spinner's thread: they name
/// the spinner, and that one names no task.
#[test]
fn samples_of_the_runtime_on_a_goroutines_thread_name_no_task() {
    let sample = |time, preempted: bool, task| SampledStack {
        pid: 1,
        tid: 2,
        time,
        weight: 1_001_001,
        functions: [
            &["runtime.sigtramp", "runtime.sigreturn__sigaction"][..usize::from(preempted) * 2],
            &[
                "sync/atomic.(*Bool).Load",
                "main.spin",
                "main.main.gowrap2",
                "runtime.goexit",
            ],
        ]
        .concat()
        .into_iter()
        .map(str::to_owned)
        .collect(),
        ending: StackEnding::Complete,
        task,
    };

    let mut spun: Vec<SampledStack> = (0..9).map(|time| sample(time, false, Some(22))).collect();
    spun.insert(3, sample(3, true, None));
    assert_eq!(spinner_of(&spun), (22, 9));

    // A sample that names another task, or a sample of the spinner's own
    // code that names none, is still wrong.
    let mut other = spun.clone();
    other[3].task = Some(7);
    assert!(std::panic::catch_unwind(|| spinner_of(&other)).is_err());
    let mut unnamed = spun.clone();
    unnamed[4].task = None;
    assert!(std::panic::catch_unwind(|| spinner_of(&unnamed)).is_err());
}

/// Each goroutine waiting in `main.receive` waits on its channel, from
/// `runtime.gopark` out to `runtime.goexit`.
fn assert_receivers_wait(profile: &Profile) {
    for wait in profile.task_waits() {
        if wait
            .functions
            .iter()
            .any(|function| function == "main.receive")
        {
            assert_eq!(wait.state, "chan receive", "{wait:?}");
            let ends = (wait.functions.first(), wait.functions.last());
            assert_eq!(
                ends,
                (
                    Some(&"runtime.gopark".into()),
                    Some(&"runtime.goexit".into())
                ),
                "{wait:?}"
            );
            assert!(wait.weight > 0, "{wait:?}");
            assert_eq!(wait.ending, StackEnding::Complete, "{wait:?}");
        }
    }
}

/// Live task snapshots read the same waits from the running program,
/// never stopping it: no pause, and no gap in its samples.
#[test]
fn live_task_snapshots_find_goroutines_waiting_without_a_pause() {
    let profile = snapshot_until(
        Command::new(Scenario::fixture("profile-go-waits")),
        true,
        |profile| receiving(profile).len() >= 2,
    );
    let (taken, paused, failed) = profile.task_snapshots();
    assert!(
        taken >= 2 && paused == 0 && failed == 0,
        "{taken} {paused} {failed}: {:?}",
        profile.task_snapshot_problems()
    );
    assert!(profile.gaps().is_empty(), "{:?}", profile.gaps());
    assert_eq!(profile.live_snapshots().0, taken);
    assert_receivers_wait(&profile);
}

/// A live task snapshot reads the runtime of every module the program
/// maps, not only its executable's: a C program's Go library carries the
/// runtime, whose goroutine waits until the host calls into it.
#[test]
fn live_task_snapshots_read_a_runtime_in_a_library() {
    let waiting = |profile: &Profile| {
        profile.task_waits().iter().any(|wait| {
            wait.functions
                .iter()
                .any(|function| function == "main.waiting")
        })
    };

    let profile = snapshot_until(Command::new(Scenario::fixture("go-host")), true, waiting);
    let (taken, paused, failed) = profile.task_snapshots();
    assert!(
        paused == 0 && failed == 0,
        "{taken} {paused} {failed}: {:?}",
        profile.task_snapshot_problems()
    );
    assert_eq!(profile.live_snapshots().0, taken);
    assert!(
        waiting(&profile),
        "no live snapshot found the library's goroutine: {:?}",
        profile.task_snapshot_problems()
    );
}

/// A task snapshot taken while the budget has sampling paused falls inside
/// that pause: the gaps the two leave stay apart and in time order, as a
/// saved capture needs them.
#[test]
fn snapshots_in_a_budget_pause_leave_gaps_apart() {
    // The program waits on its input, so it never exits while a snapshot
    // holds it; the recording ends it between snapshots.
    let (reader, _writer) = std::io::pipe().expect("pipe");
    let mut command = Command::new(Scenario::fixture("profile-go-waits"));
    command
        .stdin(reader)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let options = RecordOptions {
        budget: Some(Budget {
            cpu: 0.001,
            period: Duration::from_millis(50),
        }),
        task_snapshots: Some(Duration::from_millis(50)),
        ..RecordOptions::default()
    };

    let stop = AtomicBool::new(false);
    let recording = record_watched(
        command,
        &options,
        &mut Watch {
            every: Duration::from_millis(20),
            publish: &mut |profile| {
                if profile.task_snapshots().0 >= 8 && profile.paused(GapReason::Budget).len() >= 3 {
                    stop.store(true, Ordering::Relaxed);
                }
            },
            stop: &stop,
            save: None,
        },
    )
    .expect("record");
    let gaps = recording.profile.gaps();
    assert!(
        gaps.windows(2).all(|pair| pair[0].1 <= pair[1].0),
        "{gaps:?}"
    );
    let snapshots = recording.profile.paused(GapReason::TaskSnapshot);
    let budget = recording.profile.paused(GapReason::Budget);
    assert!(
        snapshots
            .iter()
            .any(|&(start, _)| budget.iter().any(|&(from, to)| from < start && start <= to)),
        "no snapshot fell in a budget pause: {gaps:?}"
    );
}

/// How long a recording holds its program for a profile to be ready
/// before it releases it anyway, and the test fails on what it lacks.
const READY_DEADLINE: Duration = Duration::from_secs(20);

/// Records `command` with a task snapshot every 50 ms, `live` or stopping
/// it, holding it until a profile so far is `ready`, or the deadline, then
/// writing a line to release it.
fn snapshot_until(
    mut command: Command,
    live: bool,
    mut ready: impl FnMut(&Profile) -> bool,
) -> Profile {
    let (reader, mut writer) = std::io::pipe().expect("pipe");
    command
        .stdin(reader)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let options = RecordOptions {
        task_snapshots: Some(Duration::from_millis(50)),
        live_snapshots: live,
        ..RecordOptions::default()
    };

    let stop = AtomicBool::new(false);
    let mut released = false;
    let deadline = std::time::Instant::now() + READY_DEADLINE;
    let recording = record_watched(
        command,
        &options,
        &mut Watch {
            every: Duration::from_millis(20),
            publish: &mut |profile| {
                if !released && (ready(&profile) || std::time::Instant::now() > deadline) {
                    writer.write_all(b"\n").expect("release the program");
                    released = true;
                }
            },
            stop: &stop,
            save: None,
        },
    )
    .expect("record");
    assert!(recording.status.success(), "{}", recording.status);
    recording.profile
}

/// The times of the snapshots that found three distinct goroutines waiting
/// to receive in `main.receive`.
/// Whether a task snapshot has found the waits fixture's spinner once it
/// labeled itself.
fn spins_labeled(profile: &Profile) -> bool {
    profile
        .tasks()
        .iter()
        .any(|task| task.labels == [("phase".to_owned(), "spin".to_owned())])
}

fn receiving(profile: &Profile) -> Vec<i64> {
    let mut found: std::collections::BTreeMap<i64, std::collections::BTreeSet<u64>> =
        std::collections::BTreeMap::new();
    for wait in profile.task_waits() {
        if wait
            .functions
            .iter()
            .any(|function| function == "main.receive")
        {
            found.entry(wait.time).or_default().insert(wait.task);
        }
    }

    found
        .into_iter()
        .filter(|(_, tasks)| tasks.len() == 3)
        .map(|(time, _)| time)
        .collect()
}

/// A task snapshot reads a suspended tokio task's chain of awaits: the
/// future it awaits, then each async function out to its root, whether it
/// stops the program or reads it live.
#[test]
fn task_snapshots_follow_a_tokio_tasks_awaits() {
    for live in [false, true] {
        follows_a_tokio_tasks_awaits(live);
    }
}

fn follows_a_tokio_tasks_awaits(live: bool) {
    let at_gate = |profile: &Profile| {
        profile
            .task_waits()
            .into_iter()
            .filter(|wait| {
                wait.functions
                    .iter()
                    .any(|function| function == "cancel::waiting::{closure#0}")
            })
            .collect::<Vec<_>>()
    };

    let mut command = Command::new(Scenario::fixture("tokio-cancel-o0"));
    command.arg("hold");

    // Tokio is found through its threads' thread pointers, which a live
    // snapshot learns from the C library's lists of its threads.
    let profile = snapshot_until(command, live, |profile| !at_gate(profile).is_empty());
    let (taken, _, failed) = profile.task_snapshots();
    let (read_live, _) = profile.live_snapshots();
    assert_eq!(failed, 0, "{:?}", profile.task_snapshot_problems());
    assert_eq!(
        read_live,
        if live { taken } else { 0 },
        "a live recording never stops the program"
    );
    let waits = at_gate(&profile);
    assert!(!waits.is_empty());
    for wait in waits {
        assert_eq!(
            wait.functions,
            [
                "awaiting tokio::sync::oneshot::Receiver<u64>",
                "cancel::waiting::{closure#0}",
                "cancel::alone::{closure#0}",
            ],
            "{wait:?}"
        );
        assert_eq!(wait.ending, StackEnding::Complete, "{wait:?}");
        assert!(wait.weight > 0, "{wait:?}");
    }
}

/// Records the probes fixture with `arguments`, and what it printed.
fn record_probes(arguments: &[&str], options: &RecordOptions) -> (Profile, String) {
    let (mut reader, writer) = std::io::pipe().expect("pipe");
    let mut command = Command::new(Scenario::fixture("probes"));
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(Stdio::null());
    let recording = record(command, options).expect("record");
    assert!(recording.status.success(), "{}", recording.status);
    let mut printed = String::new();
    std::io::Read::read_to_string(&mut reader, &mut printed).expect("read what it printed");
    (recording.profile, printed)
}

/// Each depth's zones of one thread, as names, and labels as `key=value`.
fn zone_depths(profile: &Profile, tid: u32) -> Vec<Vec<String>> {
    let mut depths: Vec<Vec<String>> = Vec::new();
    for zone in profile.zones().into_iter().filter(|zone| zone.tid == tid) {
        let depth = zone.depth as usize;
        depths.resize_with(depths.len().max(depth + 1), Vec::new);
        depths[depth].push(match zone.value {
            Some(value) => format!("{}={value}", zone.name),
            None => zone.name,
        });
    }

    depths
}

/// Zones and labels nest exactly as the program's scopes did, each thread
/// its own tree, on the clock its samples share, and the program's plots,
/// messages, and frames land on its thread.
#[test]
fn instrumented_zones_nest_exactly_as_the_program_ran_them() {
    let (profile, _) = record_probes(&["zones"], &RecordOptions::default());
    let counts = profile.probe_counts();
    assert!(!counts.lost(), "{counts:?}");
    let zones = profile.zones();
    let tid = |name: &str| {
        zones
            .iter()
            .find(|zone| zone.name == name)
            .map(|zone| zone.tid)
            .expect("its zone")
    };

    assert_eq!(
        zone_depths(&profile, tid("outer")),
        [
            vec!["round=0", "round=1", "round=2"],
            vec!["outer"; 3],
            ["a", "b"].repeat(3),
            vec!["leaf"; 3],
        ]
    );
    assert_eq!(
        zone_depths(&profile, tid("work")),
        [vec!["work"], vec!["step"; 2]]
    );
    assert_ne!(tid("outer"), tid("work"));

    // Each zone lies in the one that holds it, and in the recording.
    let (start, end) = profile.span();
    for zone in &zones {
        assert_eq!(zone.flags, 0, "{zone:?}");
        assert!(
            start <= zone.start && zone.end <= end,
            "{zone:?} beyond {start}..{end}"
        );
        if zone.depth > 0 {
            assert!(
                zones.iter().any(|parent| parent.tid == zone.tid
                    && parent.depth + 1 == zone.depth
                    && parent.start <= zone.start
                    && zone.end <= parent.end),
                "{zone:?} lies in no zone"
            );
        }
    }

    let plotted: Vec<f64> = profile.plots().iter().map(|plot| plot.value).collect();
    assert_eq!(plotted, [0.0, 1.0, 2.0]);
    let markers = profile.program_markers();
    let messages: Vec<&str> = markers
        .iter()
        .filter_map(|marker| marker.text.as_deref())
        .collect();
    assert_eq!(messages, ["round 0 of 3", "round 1 of 3", "round 2 of 3"]);
    let frames = markers
        .iter()
        .filter(|marker| marker.kind == MarkerKind::Frame && marker.name == "frame")
        .count();
    assert_eq!(frames, 3);
    assert!(markers.iter().all(|marker| marker.tid == tid("outer")));
}

/// A thread that writes faster than microscope reads overwrites its oldest
/// records. Every record is read or counted lost, and every event read is
/// in a zone: a whole one, or one flagged for the end it lost.
#[test]
fn every_instrumentation_record_is_read_or_counted_lost() {
    let options = RecordOptions {
        off_cpu: false,
        ..RecordOptions::default()
    };

    let (profile, _) = record_probes(&["burst", "200000"], &options);
    let counts = profile.probe_counts();
    assert_eq!(counts.events + counts.overwritten, 400_000, "{counts:?}");
    assert_eq!(
        (counts.no_ring, counts.interrupted, counts.malformed),
        (0, 0, 0)
    );
    let zones = profile.zones();
    let whole = zones.iter().filter(|zone| zone.flags == 0).count() as u64;
    let flagged = zones.len() as u64 - whole;
    assert_eq!(2 * whole + flagged, counts.events, "{counts:?}");
    assert!(zones.iter().all(|zone| zone.name == "tick"));
}

/// Threads that come and go reuse the rings of those that went: each
/// thread's zone is whole, and any no ring could take is counted.
#[test]
fn threads_that_come_and_go_each_keep_their_zones() {
    let options = RecordOptions {
        off_cpu: false,
        ..RecordOptions::default()
    };

    let (profile, _) = record_probes(&["churn", "2000"], &options);
    let counts = profile.probe_counts();
    let lives: Vec<_> = profile
        .zones()
        .into_iter()
        .filter(|zone| zone.name == "life")
        .collect();
    assert!(lives.iter().all(|zone| zone.flags == 0 && zone.depth == 0));
    let threads: std::collections::BTreeSet<u32> = lives.iter().map(|zone| zone.tid).collect();
    assert_eq!(threads.len(), lives.len());
    assert_eq!(lives.len() as u64 + counts.no_ring / 2, 2000, "{counts:?}");
    assert_eq!((counts.overwritten, counts.malformed), (0, 0));
}

/// A forked child records nothing: it is not listened to, and its sites
/// are off, while its parent's zone around the fork stays whole.
#[test]
fn a_forked_child_writes_no_instrumentation() {
    let (profile, printed) = record_probes(&["fork"], &RecordOptions::default());
    assert_eq!(printed, "child listening: false\n");
    let zones = profile.zones();
    let names: Vec<&str> = zones.iter().map(|zone| zone.name.as_str()).collect();
    assert_eq!(names, ["parent"]);
    assert_eq!(zones[0].flags, 0);
    assert!(!profile.probe_counts().lost());
}

/// A program that executes itself again is listened to again: the new
/// program's sites are switched on as the first's were.
#[test]
fn a_program_that_executes_itself_is_listened_to_again() {
    let (profile, _) = record_probes(&["exec"], &RecordOptions::default());
    let zones = profile.zones();
    let names: Vec<&str> = zones.iter().map(|zone| zone.name.as_str()).collect();
    assert_eq!(names, ["after"]);
    assert_eq!(zones[0].flags, 0);
    assert!(!profile.probe_counts().lost());
}

/// A program's trigger saves the window as it is, as the recording goes
/// on.
#[test]
fn a_programs_trigger_saves_the_window() {
    let mut command = Command::new(Scenario::fixture("probes"));
    command
        .arg("zones")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let options = RecordOptions {
        window: Some(Duration::from_secs(10)),
        ..RecordOptions::default()
    };

    let (trigger, stop) = (AtomicBool::new(false), AtomicBool::new(false));
    let mut saved = Vec::new();
    let recording = record_watched(
        command,
        &options,
        &mut Watch {
            every: Duration::from_millis(20),
            publish: &mut |_| {},
            stop: &stop,
            save: Some(Save {
                trigger: &trigger,
                to: &mut |profile| saved.push(profile),
            }),
        },
    )
    .expect("record");
    assert!(recording.status.success(), "{}", recording.status);
    assert_eq!(saved.len(), 1);
    let markers = saved[0].program_markers();
    let triggers: Vec<&str> = markers
        .iter()
        .filter(|marker| marker.kind == MarkerKind::Trigger)
        .map(|marker| marker.name.as_str())
        .collect();
    assert_eq!(triggers, ["done"]);
    let outer = saved[0]
        .zones()
        .into_iter()
        .filter(|zone| zone.name == "outer")
        .count();
    assert_eq!(outer, 3);
}

/// Only the sites asked for are switched on.
#[test]
fn a_recording_switches_on_only_the_sites_it_names() {
    let options = RecordOptions {
        probes: ProbeSelection::Named(vec!["outer".into(), "le*".into()]),
        ..RecordOptions::default()
    };

    let (profile, _) = record_probes(&["zones"], &options);
    let names: std::collections::BTreeSet<String> =
        profile.zones().into_iter().map(|zone| zone.name).collect();
    assert_eq!(names, ["leaf".to_owned(), "outer".to_owned()].into());
    assert!(profile.plots().is_empty() && profile.program_markers().is_empty());
}

/// A profile recorded with its symbols left for later holds only build
/// ids and addresses. Opened where its program's file is gone, a module
/// keeps its addresses and says why, until a debug directory holds a file
/// with its build id, from which its frames get their names.
#[test]
fn symbols_left_for_later_come_from_a_debug_file_with_the_build_id() {
    let scratch = support::ScratchDir::new("profiler-symbols-later");
    let program = scratch.path().join("basic");
    std::fs::copy(Scenario::fixture("split/basic-build-id.full"), &program)
        .expect("copy the program");
    let options = RecordOptions {
        symbols_later: true,
        ..calls_of("breakpoint_target")
    };

    let mut command = Command::new(&program);
    command.stdin(Stdio::null()).stdout(Stdio::null());
    let mut profile = record(command, &options).expect("record").profile;
    std::fs::remove_file(&program).expect("delete the program");
    let named = |profile: &Profile| {
        profile
            .stacks()
            .into_iter()
            .map(|stack| stack.functions.into_iter().take(2).collect::<Vec<_>>())
            .collect::<Vec<_>>()
    };

    assert!(profile.symbols_later());
    assert!(
        named(&profile).iter().flatten().all(|name| name != "main"),
        "{:?}",
        named(&profile)
    );

    let basic = |found: &[microscope::profiler::ModuleSymbols]| {
        found
            .iter()
            .find(|module| module.module.ends_with("/basic"))
            .cloned()
            .expect("the program's module")
    };

    let found = profile.symbolize(&microscope::DebugFileOptions::default());
    assert_eq!(
        basic(&found).source,
        Err("its file is gone, and no debug directory or server has its build id".to_owned())
    );

    let root = Scenario::fixture("split/debug-root");
    let found = profile.symbolize(&microscope::DebugFileOptions {
        directories: vec![root.clone()],
        ..microscope::DebugFileOptions::default()
    });
    let module = basic(&found);
    assert!(
        module
            .source
            .as_ref()
            .is_ok_and(|file| file.starts_with(&root)),
        "{module:?}"
    );

    // The function, main at each of its two calls, and _start.
    assert_eq!(module.frames, 4, "{module:?}");
    assert_eq!(
        named(&profile),
        vec![vec!["breakpoint_target".to_owned(), "main".to_owned()]; 2]
    );
    assert!(!profile.symbols_later());
}
