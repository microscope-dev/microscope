//! Profiles of Node programs: each thread's JavaScript frames, by line,
//! from the engines that read them beside the samples, held to the frames
//! V8's own call sites reported (`tests/fixtures/node/truth.js`).

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use microscope::profiler::{
    Granularity, Look, Profile, PythonEngine, Query, RecordOptions, Source, Watch, record_watched,
};

use crate::support::truth::Report;
use crate::support::{ScratchDir, node};
use crate::truth::{Thread, threads};

/// The entry the profiler tests run on: the reader is shared, and these
/// test its use.
pub const ENTRY: &str = "26";

/// How long a recording holds its program for a profile to be ready
/// before it releases it anyway, and the test fails on what it lacks.
const READY_DEADLINE: Duration = Duration::from_secs(30);

/// The snapshots a recording must hold, past its deadline, before it is
/// released unready, and how long it may wait for them (see
/// `tests/python/profile.rs`).
const ENOUGH_SNAPSHOTS: u64 = 10;
const SNAPSHOT_DEADLINE: Duration = Duration::from_secs(100);

/// How often each engine reads the program.
const EVERY: Duration = Duration::from_millis(100);

/// Each path of `source`'s call tree in `look` that weight ends at,
/// outermost first, by line, with the weight that ends there.
pub fn paths(profile: &Profile, source: Source, look: Look) -> BTreeMap<Vec<String>, u64> {
    let keys = profile.keys(Granularity::Line);
    let tree = keys.aggregate(&Query {
        source,
        look,
        ..Query::default()
    });

    (0..tree.len())
        .filter(|node| tree.own[*node] > 0)
        .map(|node| {
            let mut path = Vec::new();
            let mut at = u32::try_from(node).expect("a node");
            while let Some(&key) = tree.key.get(at as usize) {
                path.push(keys.name(key));
                at = tree.parent[at as usize];
            }

            path.reverse();
            (path, tree.own[node])
        })
        .collect()
}

/// A frame V8 reported, as a profile names a line: its function, its
/// file's last component, and its line.
fn line_key(function: &str, file: &str, line: Option<u32>) -> String {
    let function = if function == "-" { "" } else { function };
    let file = file.rsplit('/').next().unwrap_or(file);
    format!("{function} {file}:{}", line.unwrap_or(0))
}

/// The frames of each thread V8 reported, outermost first, as the paths of
/// a profile's interpreted look begin.
pub fn expected(reported: &[Thread]) -> Vec<(String, Vec<String>)> {
    reported
        .iter()
        .map(|thread| {
            let frames = thread
                .frames
                .iter()
                .rev()
                .map(|frame| line_key(&frame.function, &frame.file, frame.line))
                .collect();
            (thread.name.clone(), frames)
        })
        .collect()
}

/// Each thread whose frames begin no path of `paths`, with its frames and
/// the paths.
pub fn missing(
    expected: &[(String, Vec<String>)],
    paths: &BTreeMap<Vec<String>, u64>,
) -> Vec<String> {
    expected
        .iter()
        .filter(|(_, frames)| !paths.keys().any(|path| path.starts_with(frames)))
        .map(|(name, frames)| format!("thread {name}: no path begins {frames:#?}"))
        .collect()
}

/// Records `fixture` on the entry with `options`, holding it at its
/// checkpoint until its profile so far is `ready` for what it reported, or
/// the deadline, then releasing it: the profile, and what it reported.
pub fn record_parked(
    fixture: &str,
    checkpoint: &str,
    options: &RecordOptions,
    mut ready: impl FnMut(&Profile, &[Thread]) -> bool,
) -> (Profile, Vec<Thread>) {
    let scratch = ScratchDir::new("node-profile");
    let output = scratch.path().join("output");
    let (reader, mut writer) = std::io::pipe().expect("pipe");
    let mut command = Command::new(node::node(ENTRY));
    command
        .arg(node::fixture(fixture))
        .env_clear()
        .stdin(reader)
        .stdout(std::fs::File::create(&output).expect("the fixture's output"))
        .stderr(Stdio::null());

    let stop = AtomicBool::new(false);
    let mut released = false;
    let started = Instant::now();
    let mut reported = Vec::new();
    let recording = record_watched(
        command,
        options,
        &mut Watch {
            every: Duration::from_millis(50),
            publish: &mut |profile| {
                if reported.is_empty() {
                    reported = parked(&output, checkpoint);
                }
                let waited = started.elapsed();
                let given_up = waited > READY_DEADLINE
                    && (profile.task_snapshots().0 >= ENOUGH_SNAPSHOTS
                        || waited > SNAPSHOT_DEADLINE);
                if !released && ((!reported.is_empty() && ready(&profile, &reported)) || given_up) {
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
    if reported.is_empty() {
        reported = parked(&output, checkpoint);
    }
    (recording.profile, reported)
}

/// The threads the fixture writing `output` reported at `checkpoint`, once
/// it has.
fn parked(output: &Path, checkpoint: &str) -> Vec<Thread> {
    let text = std::fs::read_to_string(output).unwrap_or_default();
    if !text.contains("\nREADY\n") {
        return Vec::new();
    }
    Report::parse(&text)
        .checkpoint(checkpoint)
        .map(threads)
        .unwrap_or_default()
}

/// Snapshots that stop the program find each thread where it parks, its
/// JavaScript frames by line in the interpreted look, as V8 reported them.
#[test]
fn stopped_snapshots_find_each_threads_javascript_lines() {
    let options = RecordOptions {
        python: Some(PythonEngine::Snapshot { every: EVERY }),
        ..RecordOptions::default()
    };
    let ready = |profile: &Profile, reported: &[Thread]| {
        missing(
            &expected(reported),
            &paths(profile, Source::Waits, Look::Python),
        )
        .is_empty()
    };
    let (profile, reported) = record_parked("stacks.js", "parked", &options, ready);
    assert_eq!(reported.len(), 3, "{reported:#?}");

    let (taken, paused, _) = profile.task_snapshots();
    assert!(taken > 0 && paused > 0, "{taken} {paused}");
    let paths = paths(&profile, Source::Waits, Look::Python);
    let missing = missing(&expected(&reported), &paths);
    assert!(missing.is_empty(), "{}\n{paths:#?}", missing.join("\n"));
}

/// The comparison fails on each lie it exists to catch: a dropped frame, a
/// wrong line, and a wrong function.
#[test]
fn the_path_comparison_fails_on_each_lie() {
    let truth = vec![(
        "main".to_owned(),
        vec!["outer a.js:9".to_owned(), "inner a.js:3".to_owned()],
    )];
    let path = |frames: &[&str]| -> BTreeMap<Vec<String>, u64> {
        let mut native = frames
            .iter()
            .map(|frame| (*frame).to_owned())
            .collect::<Vec<_>>();
        native.push("read".to_owned());
        std::iter::once((native, 1)).collect()
    };
    assert!(missing(&truth, &path(&["outer a.js:9", "inner a.js:3"])).is_empty());
    for lie in [
        path(&["inner a.js:3"]),
        path(&["outer a.js:9", "inner a.js:4"]),
        path(&["outer a.js:9", "other a.js:3"]),
    ] {
        assert_eq!(missing(&truth, &lie).len(), 1, "{lie:?}");
    }
}

/// What a flight recording shows of a write to the program, or of a turn
/// to its inspector: a planted trap, a stored word or register, a signal
/// that opens the inspector, and a CDP command.
#[cfg(debug_assertions)]
const WRITES: [&str; 6] = [
    "install breakpoint",
    "reinstall breakpoint",
    "PTRACE_POKEDATA",
    "PTRACE_SETREGS",
    "SIGUSR1",
    " to Session",
];

/// Each line of a flight recording that shows a write.
#[cfg(debug_assertions)]
fn writes(recording: &str) -> Vec<&str> {
    recording
        .lines()
        .filter(|line| WRITES.iter().any(|write| line.contains(write)))
        .collect()
}

/// Profiling a Node program with stopped snapshots, and tracing its system
/// calls with their stacks, only read it: the flight recordings of both
/// show snapshots taken and calls traced, and no write to the program.
#[cfg(debug_assertions)]
#[test]
fn profiling_and_tracing_write_nothing() {
    let scratch = ScratchDir::new("node-read-only");
    let recording = scratch.path().join("record.log");
    let (reader, mut writer) = std::io::pipe().expect("pipe");
    let mut record = Command::new(env!("CARGO_BIN_EXE_microscope"))
        .env("MICROSCOPE_FLIGHT_RECORDING", &recording)
        .args(["record", "--python", "snapshot=0.1", "-o"])
        .arg(scratch.path().join("profile.json"))
        .arg("--")
        .arg(node::node(ENTRY))
        .arg(node::fixture("stacks.js"))
        .env_remove("NODE_OPTIONS")
        .stdin(reader)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("run microscope record");

    // Three snapshots, each a session that ended.
    let mut text = String::new();
    crate::support::wait_until("three snapshots end", || {
        text = std::fs::read_to_string(&recording).unwrap_or_default();
        text.matches("request shutdown").count() >= 3
    });
    writer.write_all(b"\n").expect("release the program");
    assert!(record.wait().expect("microscope record").success());
    let text = std::fs::read_to_string(&recording).expect("the recording");
    assert!(text.contains("request attach"), "{text}");
    let written = writes(&text);
    assert!(
        written.is_empty(),
        "the snapshots wrote:\n{}",
        written.join("\n")
    );

    let trace = scratch.path().join("trace.ndjson");
    let tracing = scratch.path().join("syscalls.log");
    let mut microscope = Command::new(env!("CARGO_BIN_EXE_microscope"));
    microscope.env("MICROSCOPE_FLIGHT_RECORDING", &tracing);
    let (reported, records) = crate::syscalls::run(
        crate::syscalls::command(microscope, &["--engine", "ptrace", "-k"], &trace, false),
        &trace,
    );
    assert!(!reported.is_empty() && !records.is_empty());
    let text = std::fs::read_to_string(&tracing).expect("the recording");
    let written = writes(&text);
    assert!(
        written.is_empty(),
        "the tracer wrote:\n{}",
        written.join("\n")
    );
}

/// The check fails on each write it exists to catch.
#[cfg(debug_assertions)]
#[test]
fn the_write_check_fails_on_each_write() {
    for write in [
        "   1.0 controller install breakpoint 0x10 in 7 for Loader",
        "   1.0 controller reinstall breakpoint 0x10 in 7",
        "   1.0 controller PTRACE_POKEDATA 7 0x10 0x0000000000000000",
        "   1.0 controller PTRACE_SETREGS 7 rip=0x10",
        "   1.0 controller tgkill 7 7 SIGUSR1",
        "   1.0 controller Runtime.evaluate to Session(1): Ok(1)",
    ] {
        let recording = format!("   0.5 controller request attach 7\n{write}\n");
        assert_eq!(writes(&recording), [write], "{write}");
    }
    assert!(writes("   0.5 controller PTRACE_SEIZE 7 exit_kill=false\n").is_empty());
}

/// Runs the fixture `busy.js` on the entry for `seconds`, with `arguments`
/// before it: the command, and where its output goes.
pub fn busy(scratch: &ScratchDir, seconds: u32) -> (Command, std::path::PathBuf) {
    let output = scratch.path().join("output");
    let mut command = Command::new(node::node(ENTRY));
    command
        .args(["--allow-natives-syntax", "--sparkplug-filter=sparkplugSpin"])
        .arg(node::fixture("busy.js"))
        .args(["--seconds", &seconds.to_string()])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&output).expect("the fixture's output"))
        .stderr(Stdio::null());
    (command, output)
}

/// The functions a thread's stack begins with: glibc's entry for the main
/// thread, and the system call that starts each other thread.
const ROOTS: [&str; 3] = ["_start", "clone3", "__GI___clone3"];

/// Whether `path` is a sample in one of V8's builtins that no unwind table
/// describes, which the stack ends at, as it must: a builtin may run
/// without a frame of its own, so its caller is not known.
fn at_builtin(path: &[String]) -> bool {
    matches!(
        path,
        [end, leaf] if end.starts_with("[no unwind information: ") && leaf.starts_with("Builtins_")
    )
}

/// Each path of `paths` that begins with no thread's root, and does not end
/// at a builtin, with its weight. A path may begin with a marker of where
/// unwinding ended past the root, in brackets, as one past `clone3` does,
/// whose caller is no function.
pub fn rootless(paths: &BTreeMap<Vec<String>, u64>) -> Vec<(String, u64)> {
    paths
        .iter()
        .filter(|(path, _)| {
            let root = path.iter().find(|frame| !frame.starts_with('['));
            !at_builtin(path)
                && !root.is_some_and(|root| {
                    ROOTS
                        .iter()
                        .any(|name| root.split(' ').next() == Some(*name))
                })
        })
        .map(|(path, weight)| (path.join(" > "), *weight))
        .collect()
}

/// How much of `paths`' sampled time entered JavaScript, through V8's
/// entry, and how much ended at a builtin no unwind table describes.
fn entered(paths: &BTreeMap<Vec<String>, u64>) -> (u64, u64) {
    let weight = |keep: &dyn Fn(&[String]) -> bool| -> u64 {
        paths
            .iter()
            .filter(|(path, _)| keep(path))
            .map(|(_, weight)| weight)
            .sum()
    };
    (
        weight(&|path| {
            path.iter()
                .any(|frame| frame.split(' ').next() == Some("Builtins_JSEntry"))
        }),
        weight(&|path| at_builtin(path)),
    )
}

/// CPU samples of a Node program spinning in JavaScript unwind through
/// the code V8 generated and the builtins it copied, which name
/// themselves, to the root of their thread, or else end visibly at a
/// builtin they were sampled in.
#[test]
fn cpu_samples_unwind_through_v8s_code_to_each_threads_root() {
    // A second of samples in JavaScript, which a loaded machine, or one
    // reading node's debug information as the kernel drops samples, takes
    // longer to give: the recording ends once it has them, or the program
    // at its own deadline.
    const ENOUGH: u64 = 1_000_000_000;
    let scratch = ScratchDir::new("node-busy");
    let (command, _) = busy(&scratch, 60);
    let stop = AtomicBool::new(false);
    let recording = record_watched(
        command,
        &RecordOptions::default(),
        &mut Watch {
            every: Duration::from_millis(100),
            publish: &mut |profile| {
                let (entered, _) = entered(&paths(&profile, Source::Samples, Look::Native));
                if entered >= ENOUGH {
                    stop.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            },
            stop: &stop,
            save: None,
        },
    )
    .expect("record");
    let stopped = stop.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        stopped || recording.status.success(),
        "{}",
        recording.status
    );
    let paths = paths(&recording.profile, Source::Samples, Look::Native);
    let total: u64 = paths.values().sum();
    let rootless = rootless(&paths);
    let short: u64 = rootless.iter().map(|(_, weight)| weight).sum();
    let (entered, at_builtins) = entered(&paths);
    let mut heaviest: Vec<_> = paths.iter().filter(|(path, _)| !at_builtin(path)).collect();
    heaviest.sort_by_key(|(_, weight)| std::cmp::Reverse(**weight));
    heaviest.truncate(5);
    assert!(
        entered >= ENOUGH,
        "{entered} of {total} ns entered JavaScript; the heaviest stacks:\n{heaviest:#?}"
    );
    // A sample may land where no unwinder can step, as in a signal
    // trampoline or a prologue: at most one in a hundred.
    assert!(
        short * 100 <= total,
        "{short} of {total} ns begin with no thread's root:\n{rootless:#?}"
    );
    assert!(
        (entered + at_builtins) * 2 >= total,
        "{entered} of {total} ns entered JavaScript"
    );
}

/// The root check fails on a stack that ends short.
#[test]
fn the_root_check_fails_on_a_short_stack() {
    let path = |frames: &[&str]| frames.iter().map(|frame| (*frame).to_owned()).collect();
    let paths: BTreeMap<Vec<String>, u64> = [
        (path(&["_start a.c:1", "main a.c:2"]), 3),
        (
            path(&["[frame pointer not kept: libc.so.6]", "__GI___clone3 b.S:9"]),
            2,
        ),
        (path(&["Builtins_JSEntry ?:0", "0x7fffc8001000 ?:0"]), 1),
        (
            path(&["[no unwind information: node]", "Builtins_AddHandler ?:0"]),
            4,
        ),
        (
            path(&["[no unwind information: node]", "0x7fffc8001000 ?:0"]),
            5,
        ),
    ]
    .into_iter()
    .collect();
    assert_eq!(
        rootless(&paths),
        [
            ("Builtins_JSEntry ?:0 > 0x7fffc8001000 ?:0".to_owned(), 1),
            (
                "[no unwind information: node] > 0x7fffc8001000 ?:0".to_owned(),
                5
            ),
        ]
    );
}

/// The frames each thread of `busy.js` reported, outermost first, as a
/// profile's interpreted look by the BPF engine keys them: by line, but
/// for a frame of compiled code, whose line the walker does not prove, and
/// the innermost frame, whose line the interpreter keeps only in a
/// register, which are keyed by their function alone.
pub fn spinning(output: &str) -> Vec<(String, Vec<String>)> {
    let mut threads: Vec<(String, String, Vec<String>)> = Vec::new();
    for fields in Report::parse(output).lines() {
        match fields {
            [kind, native, name] if kind == "spin" => {
                threads.push((native.clone(), name.clone(), Vec::new()));
            }
            [kind, native, depth, function, file, line, tier] if kind == "frame" => {
                let thread = threads
                    .iter_mut()
                    .find(|(of, ..)| of == native)
                    .expect("a frame of a reported thread");
                let compiled = tier == "sparkplug" || tier == "maglev" || tier == "turbofan";
                let key = if compiled || depth == "0" {
                    if function == "-" {
                        String::new()
                    } else {
                        function.clone()
                    }
                } else {
                    line_key(function, file, line.parse().ok())
                };
                thread.2.push(key);
            }
            _ => {}
        }
    }
    threads
        .into_iter()
        .map(|(_, name, mut frames)| {
            frames.reverse();
            (name, frames)
        })
        .collect()
}

/// Each frame of `paths` of a function the threads in `truth` run that
/// lies, one line each, of the paths sampled as `busy.js` spins, under its
/// `spinFor`, which is when V8's report holds: before then, and as the
/// program exits, Node's own functions that call it run other lines. A
/// frame lies where its key is neither one V8's report gives it, nor
/// its function alone, nor a line of `lines`, which the fixture's
/// interpreted functions call from; or it has a line but called nothing,
/// which its frame alone does not prove: the interpreter writes a frame's
/// offset only before a call, so a frame with a proven line has a callee,
/// never a bytecode handler, inside it.
pub fn misplaced(
    truth: &[(String, Vec<String>)],
    lines: &[String],
    paths: &BTreeMap<Vec<String>, u64>,
) -> Vec<String> {
    let function = |key: &str| key.split(' ').next().unwrap_or_default().to_owned();
    let mut allowed: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for key in truth.iter().flat_map(|(_, frames)| frames) {
        let name = function(key);
        if !name.is_empty() {
            let keys = allowed
                .entry(name.clone())
                .or_insert_with(|| vec![name.clone()]);
            keys.push(key.clone());
            keys.extend(lines.iter().filter(|line| function(line) == name).cloned());
        }
    }
    let mut misplaced: Vec<String> = Vec::new();
    let spinning = |path: &&Vec<String>| path.iter().any(|key| function(key) == "spinFor");
    for path in paths.keys().filter(spinning) {
        for (index, key) in path.iter().enumerate() {
            let Some(keys) = allowed.get(&function(key)) else {
                continue;
            };
            let inside = path.get(index + 1);
            let made_a_call = inside.is_some_and(|frame| {
                !(frame.starts_with("Builtins_") && frame.contains("Handler"))
            });
            if !keys.contains(key) {
                misplaced.push(key.clone());
            } else if key.contains(".js:") && !made_a_call {
                misplaced.push(format!(
                    "{key} > {}",
                    inside.map_or("nothing", String::as_str)
                ));
            }
        }
    }
    misplaced.sort();
    misplaced.dedup();
    misplaced
}

/// The keys of the lines `busy.js`'s interpreted frames call from, which
/// `output` reported: each such frame's own, and the line its function
/// loops on, the line after its own in the fixture's source, from which
/// the loop may call into the runtime.
pub fn called_lines(output: &str) -> Vec<String> {
    let source = std::fs::read_to_string(node::fixture("busy.js")).expect("read busy.js");
    let source: Vec<&str> = source.lines().collect();
    let mut lines = Vec::new();
    for fields in Report::parse(output).lines() {
        if let [kind, _, _, function, file, line, tier] = fields
            && kind == "frame"
            && (tier == "ignition" || tier == "-")
        {
            lines.push(line_key(function, file, line.parse().ok()));
            let header = format!("function {function}(");
            if let Some(at) = source.iter().position(|text| text.starts_with(&header))
                && source
                    .get(at + 1)
                    .is_some_and(|text| text.trim_start().starts_with("for ("))
            {
                lines.push(format!("{function} busy.js:{}", at + 2));
            }
        }
    }
    lines.sort();
    lines.dedup();
    lines
}

/// The share of `paths`' weight whose JavaScript frames include one the
/// engine could not name, among those that hold any.
pub fn unknown_share(paths: &BTreeMap<Vec<String>, u64>) -> (u64, u64) {
    let holding = |path: &Vec<String>| path.iter().any(|frame| frame.contains(".js:"));
    let total = paths
        .iter()
        .filter(|(path, _)| holding(path))
        .map(|(_, weight)| weight)
        .sum();
    let unknown = paths
        .iter()
        .filter(|(path, _)| {
            holding(path) && path.iter().any(|frame| frame.starts_with("<unknown>"))
        })
        .map(|(_, weight)| weight)
        .sum();
    (unknown, total)
}

/// The spin comparisons fail on each lie they exist to catch: a line
/// given to a compiled frame, one given to the innermost frame, a dropped
/// frame, and a wrong line, on the path V8 reported or any other.
#[test]
fn the_spin_comparison_fails_on_each_lie() {
    let output = "TRUTH\tspin\t7\tmain\n\
                  TRUTH\tframe\t7\t0\tinner\t/a.js\t3\tignition\n\
                  TRUTH\tframe\t7\t1\tfast\t/a.js\t5\tturbofan\n\
                  TRUTH\tframe\t7\t2\tspinFor\t/a.js\t9\t-\n";
    let truth = spinning(output);
    assert_eq!(
        truth,
        [(
            "main".to_owned(),
            vec![
                "spinFor a.js:9".to_owned(),
                "fast".to_owned(),
                "inner".to_owned()
            ]
        )]
    );
    let path = |frames: &[&str]| -> BTreeMap<Vec<String>, u64> {
        let mut native = frames
            .iter()
            .map(|frame| (*frame).to_owned())
            .collect::<Vec<_>>();
        native.push("Builtins_AddHandler".to_owned());
        std::iter::once((native, 1)).collect()
    };
    assert!(missing(&truth, &path(&["spinFor a.js:9", "fast", "inner"])).is_empty());
    let lines = ["inner a.js:2".to_owned(), "inner a.js:3".to_owned()];
    let called = |frames: &[&str]| -> BTreeMap<Vec<String>, u64> {
        let mut native = frames
            .iter()
            .map(|frame| (*frame).to_owned())
            .collect::<Vec<_>>();
        native.push("v8::internal::Runtime_StackGuard".to_owned());
        std::iter::once((native, 1)).collect()
    };
    for honest in [
        path(&["spinFor a.js:9", "fast", "inner"]),
        called(&["spinFor a.js:9", "fast", "inner a.js:2"]),
        called(&["spinFor a.js:9", "fast", "inner a.js:3"]),
        // Before the spin, as it warms up, and after.
        called(&["fast a.js:7", "inner a.js:8"]),
    ] {
        assert!(misplaced(&truth, &lines, &honest).is_empty(), "{honest:?}");
    }
    for lie in [
        path(&["spinFor a.js:9", "fast a.js:5", "inner"]),
        path(&["spinFor a.js:9", "fast", "inner a.js:2"]),
        called(&["spinFor a.js:8", "fast", "inner"]),
        called(&["spinFor a.js:9", "fast", "inner a.js:4"]),
    ] {
        assert_eq!(misplaced(&truth, &lines, &lie).len(), 1, "{lie:?}");
    }
    for lie in [
        path(&["spinFor a.js:9", "fast a.js:5", "inner"]),
        path(&["spinFor a.js:9", "fast", "inner a.js:3"]),
        path(&["spinFor a.js:9", "inner"]),
        path(&["spinFor a.js:8", "fast", "inner"]),
    ] {
        assert_eq!(missing(&truth, &lie).len(), 1, "{lie:?}");
    }
}

/// The BPF engine, whose program walks each sample's JavaScript frames,
/// held to the frames V8 reported of each spinning thread. It needs
/// `CAP_BPF` and `CAP_PERFMON`, so it runs only through `just test-bpf`.
mod bpf {
    use super::*;

    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON: run with `just test-bpf`"]
    fn samples_hold_each_spinning_threads_javascript_frames() {
        let launcher = std::env::var_os("MICROSCOPE_BPF_LAUNCHER")
            .expect("MICROSCOPE_BPF_LAUNCHER names the microscope-bpf wrapper");
        let scratch = ScratchDir::new("node-busy-bpf");
        let profile = scratch.path().join("busy.usprof");
        let (fixture, output) = busy(&scratch, 10);
        let mut command = Command::new(launcher);
        command
            .arg(env!("CARGO_BIN_EXE_microscope"))
            .args(["record", "--python", "bpf", "--output"])
            .arg(&profile)
            .arg("--")
            .arg(fixture.get_program())
            .args(fixture.get_args())
            .env_remove("NODE_OPTIONS")
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(&output).expect("the fixture's output"))
            .stderr(Stdio::piped());
        let finished = command.output().expect("run microscope");
        assert!(
            finished.status.success(),
            "{}",
            String::from_utf8_lossy(&finished.stderr)
        );

        let truth = spinning(&std::fs::read_to_string(&output).expect("the fixture's output"));
        assert_eq!(truth.len(), 2, "{truth:#?}");
        let profile = Profile::read(&profile).expect("read the profile").profile;
        let paths = paths(&profile, Source::Samples, Look::Python);
        let missing = missing(&truth, &paths);
        assert!(missing.is_empty(), "{}\n{paths:#?}", missing.join("\n"));
        let lines = called_lines(&std::fs::read_to_string(&output).expect("the fixture's output"));
        let misplaced = misplaced(&truth, &lines, &paths);
        assert!(misplaced.is_empty(), "{misplaced:#?}\n{paths:#?}");

        let (unknown, total) = unknown_share(&paths);
        eprintln!("samples with an unknown JavaScript frame: {unknown} of {total} ns");
        assert!(
            unknown * 100 <= total,
            "{unknown} of {total} ns: {paths:#?}"
        );
    }

    /// Two Node processes a shell starts, each with its own V8 and its own
    /// layout for the walker: each process's samples hold its own threads'
    /// JavaScript frames, as each reported them.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON: run with `just test-bpf`"]
    fn each_process_of_a_group_holds_its_own_javascript_frames() {
        let launcher = std::env::var_os("MICROSCOPE_BPF_LAUNCHER")
            .expect("MICROSCOPE_BPF_LAUNCHER names the microscope-bpf wrapper");
        let scratch = ScratchDir::new("node-busy-group");
        let profile = scratch.path().join("group.usprof");
        let outputs = [scratch.path().join("first"), scratch.path().join("second")];
        let run = |output: &Path| {
            format!(
                "'{}' --allow-natives-syntax --sparkplug-filter=sparkplugSpin '{}' --seconds 10 > '{}'",
                node::node(ENTRY).display(),
                node::fixture("busy.js").display(),
                output.display()
            )
        };
        let script = format!("{} & {}; wait", run(&outputs[0]), run(&outputs[1]));
        let finished = Command::new(launcher)
            .arg(env!("CARGO_BIN_EXE_microscope"))
            .args(["record", "--node", "bpf", "--output"])
            .arg(&profile)
            .args(["--", "sh", "-c", &script])
            .env_remove("NODE_OPTIONS")
            .stdin(Stdio::null())
            .output()
            .expect("run microscope");
        assert!(
            finished.status.success(),
            "{}",
            String::from_utf8_lossy(&finished.stderr)
        );

        let profile = Profile::read(&profile).expect("read the profile").profile;
        let threads = profile.threads();
        for output in &outputs {
            let text = std::fs::read_to_string(output).expect("the fixture's output");
            let pid: u32 = Report::parse(&text)
                .lines()
                .find_map(|fields| match fields {
                    [kind, pid, ..] if kind == "process" => pid.parse().ok(),
                    _ => None,
                })
                .expect("the process the fixture reported");
            let rows: Vec<u32> = threads
                .iter()
                .filter(|thread| thread.pid == pid)
                .map(|thread| thread.row)
                .collect();
            let paths = process_paths(&profile, rows);
            let truth = spinning(&text);
            assert_eq!(truth.len(), 2, "{pid}: {truth:#?}");
            let missing = missing(&truth, &paths);
            assert!(
                missing.is_empty(),
                "{pid}: {}\n{paths:#?}",
                missing.join("\n")
            );
            let misplaced = misplaced(&truth, &called_lines(&text), &paths);
            assert!(misplaced.is_empty(), "{pid}: {misplaced:#?}\n{paths:#?}");
        }
    }

    /// The paths of the samples of the threads `rows`, in the interpreted
    /// look, as [`paths`] gives them.
    fn process_paths(profile: &Profile, rows: Vec<u32>) -> BTreeMap<Vec<String>, u64> {
        let keys = profile.keys(Granularity::Line);
        let tree = keys.aggregate(&Query {
            source: Source::Samples,
            look: Look::Python,
            threads: Some(rows),
            ..Query::default()
        });
        (0..tree.len())
            .filter(|node| tree.own[*node] > 0)
            .map(|node| {
                let mut path = Vec::new();
                let mut at = u32::try_from(node).expect("a node");
                while let Some(&key) = tree.key.get(at as usize) {
                    path.push(keys.name(key));
                    at = tree.parent[at as usize];
                }
                path.reverse();
                (path, tree.own[node])
            })
            .collect()
    }
}

/// A program instrumented with the Node package is recorded as one
/// instrumented with the Rust SDK is: its zones nest as its calls did, on
/// the main thread and a worker, its labels, plots, and messages land on
/// its thread, and the sites past the library's table record nothing,
/// which the profile says.
#[test]
fn the_node_package_records_what_the_program_marks() {
    let mut command = Command::new(node::node(ENTRY));
    command
        .arg(node::fixture("probes.js"))
        .env_clear()
        .env(
            "MICROSCOPE_PROBES_LIBRARY",
            crate::support::Scenario::fixture("libmicroscope_probes_c.so"),
        )
        .env(
            "MICROSCOPE_NODE_ADDON",
            crate::support::Scenario::fixture("node-microscope.node"),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped());
    let recording =
        microscope::profiler::record(command, &RecordOptions::default()).expect("record");
    assert!(recording.status.success(), "{}", recording.status);

    let profile = recording.profile;
    let counts = profile.probe_counts();
    assert!(!counts.lost() && counts.malformed == 0, "{counts:?}");

    let zones = profile.zones();
    let named = |name: &str| {
        zones
            .iter()
            .filter(|zone| zone.name == name)
            .collect::<Vec<_>>()
    };
    let rounds = named("round");
    assert_eq!(
        rounds
            .iter()
            .map(|zone| zone.value.as_deref())
            .collect::<Vec<_>>(),
        [Some("0"), Some("1"), Some("2")],
        "{zones:?}"
    );
    let (load, parse) = (named("load"), named("parse"));
    assert_eq!((load.len(), parse.len()), (3, 3), "{zones:?}");
    for ((round, load), parse) in rounds.iter().zip(&load).zip(&parse) {
        assert!(
            round.depth < load.depth && load.depth < parse.depth,
            "{zones:?}"
        );
        assert!(
            round.start <= load.start
                && load.start <= parse.start
                && parse.end <= load.end
                && load.end <= round.end,
            "{zones:?}"
        );
    }
    assert!(named("suspends").is_empty(), "{zones:?}");
    let worker = named("worker");
    assert_eq!(worker.len(), 1, "{zones:?}");
    assert_ne!(worker[0].tid, rounds[0].tid, "{zones:?}");

    let plotted = profile
        .plots()
        .into_iter()
        .map(|plot| (plot.name, plot.value))
        .collect::<Vec<_>>();
    assert_eq!(
        plotted,
        [
            ("rows", 1.0),
            ("rows", 2.0),
            ("rows", 3.0),
            ("worker rows", 7.0)
        ]
        .map(|(name, value)| (name.to_owned(), value))
    );

    let said = profile
        .program_markers()
        .into_iter()
        .filter_map(|marker| marker.text)
        .collect::<Vec<_>>();
    assert_eq!(said, ["round 0", "round 1", "round 2"]);

    let problems = profile.probe_problems();
    assert!(
        problems
            .iter()
            .any(|(_, problem)| problem.contains("all 4096 of its run-time sites")),
        "{problems:?}"
    );
}

/// Without its addon, or with an addon but no library, the package still
/// loads, and every mark does nothing, as with no recording; `problem`
/// says why.
#[test]
fn the_node_package_without_its_native_parts_does_nothing_and_says_why() {
    let sdk = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("sdk/node");
    let script = r"
const microscope = require(process.argv[1]);
const expected = process.argv[2];
if (!String(microscope.problem).includes(expected)) {
  throw new Error(`problem: ${microscope.problem}`);
}
const load = microscope.wrap('load', () => 1);
if (load() !== 1 || microscope.listening()) {
  throw new Error('a mark did something');
}
microscope.zone('block', () => microscope.label('key', 1, () => {
  microscope.plot('rows', 1);
  microscope.message('said', 'text');
  microscope.frame();
  microscope.trigger('why');
}));
process.stdout.write('NOTHING OK\n');
";
    let addon = crate::support::Scenario::fixture("node-microscope.node");
    for (addon, expected) in [
        (
            std::path::PathBuf::from("/nonexistent/microscope.node"),
            "the addon could not be loaded",
        ),
        (addon, "the probes library could not be loaded"),
    ] {
        let output = Command::new(node::node(ENTRY))
            .args(["-e", script])
            .arg(&sdk)
            .arg(expected)
            .env_clear()
            .env("MICROSCOPE_NODE_ADDON", &addon)
            .env(
                "MICROSCOPE_PROBES_LIBRARY",
                "/nonexistent/libmicroscope_probes.so",
            )
            .output()
            .expect("run node");
        let printed = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && printed.contains("NOTHING OK"),
            "{expected}: {printed}{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
