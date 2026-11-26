use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use microscope::profiler::{
    Granularity, Look, Profile, Query, RecordOptions, SampledStack, Source, StackEnding, Watch,
    record_watched,
};

use crate::support::profile::Snapshotting;
use crate::support::truth::Report;
use crate::support::{ScratchDir, node};
use crate::truth::{Thread, threads};

pub const ENTRY: &str = "26";

use crate::support::released::READY_DEADLINE;

const EVERY: Duration = Duration::from_millis(100);

pub fn paths(profile: &Profile, source: Source, look: Look) -> BTreeMap<Vec<String>, u64> {
    paths_of(
        profile,
        &Query {
            source,
            look,
            ..Query::default()
        },
    )
}

pub fn paths_of(profile: &Profile, query: &Query) -> BTreeMap<Vec<String>, u64> {
    let keys = profile.keys(Granularity::Line);
    let tree = keys.aggregate(query);

    (0..tree.len())
        .filter(|node| tree.own[*node] > 0)
        .map(|node| {
            let mut path = Vec::new();
            let mut at = u32::try_from(node).expect("a node");
            while let Some(&key) = tree.key.get(at as usize) {
                path.push(reported_name(keys.name(key)));
                at = tree.parent[at as usize];
            }

            path.reverse();
            (path, tree.own[node])
        })
        .collect()
}

pub fn reported_name(name: String) -> String {
    match name
        .strip_prefix("<anonymous ")
        .and_then(|rest| rest.split_once('>'))
    {
        Some((_, rest)) => format!("<anonymous>{rest}"),
        None => name,
    }
}

pub fn line_key(function: &str, file: &str, line: Option<u32>) -> String {
    let function = if function == "-" {
        "<anonymous>"
    } else {
        function
    };
    let file = file.rsplit('/').next().unwrap_or(file);
    format!("{function} {file}:{}", line.unwrap_or(0))
}

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

pub fn record_parked(
    fixture: &str,
    checkpoint: &str,
    options: &RecordOptions,
    mut ready: impl FnMut(&Profile, &[Thread]) -> bool,
) -> (Profile, Vec<Thread>) {
    let mut command = Command::new(node::node(ENTRY));
    command.arg(node::fixture(fixture));
    let (profile, reported, _) = record_released(
        command,
        options,
        |text| Some(parked(text, checkpoint)).filter(|threads| !threads.is_empty()),
        |profile, reported| ready(profile, reported),
    );
    (profile, reported.unwrap_or_default())
}

pub fn record_released<T>(
    mut command: Command,
    options: &RecordOptions,
    report: impl Fn(&str) -> Option<T>,
    mut ready: impl FnMut(&Profile, &T) -> bool,
) -> (Profile, Option<T>, String) {
    let scratch = ScratchDir::new("node-profile");
    let output = scratch.path().join("output");
    let errors = scratch.path().join("errors");
    command
        .env_clear()
        .stdout(std::fs::File::create(&output).expect("the fixture's output"))
        .stderr(std::fs::File::create(&errors).expect("the fixture's errors"));

    let mut reported = None;
    let read = || report(&std::fs::read_to_string(&output).unwrap_or_default());
    let recording = crate::support::released::record_released(
        command,
        options,
        crate::support::released::when_snapshotted(|profile| {
            if reported.is_none() {
                reported = read();
            }
            reported
                .as_ref()
                .is_some_and(|reported| ready(profile, reported))
        }),
    );

    let text = std::fs::read_to_string(&output).unwrap_or_default();
    assert!(
        recording.status.success(),
        "{}\n{}\n{text}",
        recording.status,
        std::fs::read_to_string(&errors).unwrap_or_default()
    );
    (recording.profile, reported.or_else(|| report(&text)), text)
}

fn parked(text: &str, checkpoint: &str) -> Vec<Thread> {
    if !text.contains("\nREADY\n") {
        return Vec::new();
    }
    Report::parse(text)
        .checkpoint(checkpoint)
        .map(threads)
        .unwrap_or_default()
}

#[test]
fn stopped_snapshots_find_each_threads_javascript_lines() {
    let options = Snapshotting::Stopped(EVERY).options();
    let ready = |profile: &Profile, reported: &[Thread]| {
        missing(
            &expected(reported),
            &paths(profile, Source::Waits, Look::Code),
        )
        .is_empty()
    };
    let (profile, reported) = record_parked("stacks.js", "parked", &options, ready);
    assert_eq!(reported.len(), 3, "{reported:#?}");

    let (taken, paused, _) = profile.task_snapshots();
    assert!(taken > 0 && paused > 0, "{taken} {paused}");

    for (thread, frames) in reported.iter().zip(expected(&reported)) {
        let rows: Vec<u32> = profile
            .tasks()
            .iter()
            .filter(|task| task.noun == "thread" && task.number == thread.native)
            .map(|task| task.row)
            .collect();
        let paths = paths_of(
            &profile,
            &Query {
                source: Source::Waits,
                look: Look::Code,
                tasks: Some(rows),
                ..Query::default()
            },
        );
        let missing = missing(std::slice::from_ref(&frames), &paths);
        assert!(missing.is_empty(), "{}\n{paths:#?}", missing.join("\n"));
    }

    let waits = Query {
        source: Source::Waits,
        look: Look::Code,
        ..Query::default()
    };
    let code = profile.disassemble(&waits, "mainOuter");
    assert!(
        code.as_ref()
            .is_err_and(|why| why.contains("Python or JavaScript")),
        "{:?}",
        code.map(|code| code.blocks.len())
    );
}

#[cfg(debug_assertions)]
const WRITES: [&str; 6] = [
    "install breakpoint",
    "reinstall breakpoint",
    "PTRACE_POKEDATA",
    "PTRACE_SETREGS",
    "SIGUSR1",
    " to Session",
];

#[cfg(debug_assertions)]
fn writes(recording: &str) -> Vec<&str> {
    recording
        .lines()
        .filter(|line| WRITES.iter().any(|write| line.contains(write)))
        .collect()
}

#[cfg(debug_assertions)]
#[test]
fn profiling_and_tracing_write_nothing() {
    use std::io::Write as _;

    let scratch = ScratchDir::new("node-read-only");
    let recording = scratch.path().join("record.log");
    let (reader, mut writer) = std::io::pipe().expect("pipe");
    let mut record = Command::new(env!("CARGO_BIN_EXE_microscope"))
        .env("MICROSCOPE_FLIGHT_RECORDING", &recording)
        .args(["record", "--snapshots", "0.1", "--allow-stopping", "-o"])
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

    let trace = scratch.path().join("trace.usprof");
    let tracing = scratch.path().join("syscalls.log");
    let mut microscope = Command::new(env!("CARGO_BIN_EXE_microscope"));
    microscope.env("MICROSCOPE_FLIGHT_RECORDING", &tracing);
    let (reported, records) =
        crate::syscalls::run(crate::syscalls::command(microscope, &trace), &trace);
    assert!(!reported.is_empty() && !records.is_empty());
    let text = std::fs::read_to_string(&tracing).expect("the recording");
    let written = writes(&text);
    assert!(
        written.is_empty(),
        "the tracer wrote:\n{}",
        written.join("\n")
    );
}

pub fn busy(scratch: &ScratchDir, seconds: u32) -> (Command, std::path::PathBuf) {
    busy_on(ENTRY, scratch, seconds)
}

pub fn busy_on(entry: &str, scratch: &ScratchDir, seconds: u32) -> (Command, std::path::PathBuf) {
    let output = scratch.path().join("output");
    let mut command = Command::new(node::node(entry));
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

const ROOTS: [&str; 3] = ["_start", "clone3", "__GI___clone3"];

fn at_builtin(path: &[String]) -> bool {
    matches!(
        path,
        [end, leaf] if end.starts_with("[no unwind information: ") && leaf.starts_with("Builtins_")
    )
}

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

#[test]
fn cpu_samples_unwind_through_v8s_code_to_each_threads_root() {
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
    assert!(
        short * 100 <= total,
        "{short} of {total} ns begin with no thread's root:\n{rootless:#?}"
    );
    assert!(
        (entered + at_builtins) * 2 >= total,
        "{entered} of {total} ns entered JavaScript"
    );
}

#[test]
fn v8_without_its_debugger_constants_unwinds_through_its_code() {
    const ENOUGH: usize = 1000;
    let scratch = ScratchDir::new("node-busy-without-postmortem");
    let (command, _) = busy_on("26-without-postmortem", &scratch, 60);
    let stop = AtomicBool::new(false);
    let started = Instant::now();
    let recording = record_watched(
        command,
        &RecordOptions::default(),
        &mut Watch {
            every: Duration::from_millis(100),
            publish: &mut |profile| {
                if profile.samples() >= ENOUGH || started.elapsed() > READY_DEADLINE {
                    stop.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            },
            stop: &stop,
            save: None,
        },
    )
    .expect("record");
    assert!(
        recording.profile.samples() >= ENOUGH,
        "{} samples",
        recording.profile.samples()
    );
    let paths = paths(&recording.profile, Source::Samples, Look::Native);
    let total: u64 = paths.values().sum();
    let rootless = rootless(&paths);
    let short: u64 = rootless.iter().map(|(_, weight)| weight).sum();
    assert!(total > 0, "no samples");
    assert!(
        short * 100 <= total,
        "{short} of {total} ns begin with no thread's root:\n{rootless:#?}"
    );
    let (entered, at_builtins) = entered(&paths);
    assert!(
        (entered + at_builtins) * 2 >= total,
        "{entered} of {total} ns entered JavaScript"
    );
}

#[test]
fn perf_map_names_v8s_code_that_samples_unwind_through() {
    const ENOUGH: usize = 20;
    let named = |stack: &SampledStack| {
        stack.ending == StackEnding::Complete
            && stack.functions.iter().any(|name| name.starts_with("JS:"))
    };
    let scratch = ScratchDir::new("node-perf-map");
    let output = scratch.path().join("output");
    let mut command = Command::new(node::node(ENTRY));
    command
        .args([
            "--perf-basic-prof",
            "--allow-natives-syntax",
            "--sparkplug-filter=sparkplugSpin",
        ])
        .arg(node::fixture("busy.js"))
        .args(["--seconds", "60"])
        .current_dir(scratch.path())
        .env_clear()
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&output).expect("the fixture's output"))
        .stderr(Stdio::null());
    let stop = AtomicBool::new(false);
    let started = Instant::now();
    let recording = record_watched(
        command,
        &RecordOptions::default(),
        &mut Watch {
            every: Duration::from_millis(100),
            publish: &mut |profile| {
                let enough = profile.stacks().iter().filter(|stack| named(stack)).count();
                if enough >= ENOUGH || started.elapsed() > READY_DEADLINE {
                    stop.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            },
            stop: &stop,
            save: None,
        },
    );
    let text = std::fs::read_to_string(&output).unwrap_or_default();
    let mut map = String::new();
    for (name, pid) in spinners(&text) {
        if name == "main" {
            let path = format!("/tmp/perf-{pid}.map");
            map = std::fs::read_to_string(&path).unwrap_or_default();
            let _ = std::fs::remove_file(&path);
        }
    }
    let profile = recording.expect("record").profile;
    let keys = profile.keys(Granularity::Function);
    let ordinary: Vec<String> = (0..u32::try_from(keys.count()).expect("keys"))
        .filter(|&key| {
            keys.name(key).starts_with("JS:") && keys.role(key) != microscope::CodeRole::Interpreter
        })
        .map(|key| keys.name(key))
        .collect();
    assert!(ordinary.is_empty(), "{ordinary:?}");
    let stacks = profile.stacks();
    assert!(!map.is_empty(), "node wrote no perf map");
    let lost = known_by_address(&map, &stacks);
    assert!(
        lost.len() * 20 <= stacks.len(),
        "{} of {} samples have a frame the perf map names known by its address: {:?}",
        lost.len(),
        stacks.len(),
        lost.first().map(|stack| &stack.functions)
    );
    let through = stacks.iter().filter(|stack| named(stack)).count();
    let any_named = stacks
        .iter()
        .flat_map(|stack| &stack.functions)
        .any(|name| name.starts_with("JS:"));
    let complete = stacks
        .iter()
        .filter(|stack| stack.ending == StackEnding::Complete)
        .count();
    assert!(
        through >= ENOUGH,
        "{through} of {} samples have a frame the perf map names and unwind to their root; \
         any frame named: {any_named}, complete stacks: {complete}",
        stacks.len()
    );
}

fn known_by_address<'a>(map: &str, stacks: &'a [SampledStack]) -> Vec<&'a SampledStack> {
    let functions: Vec<(u64, u64)> = map
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, ' ');
            let start = u64::from_str_radix(fields.next()?, 16).ok()?;
            let size = u64::from_str_radix(fields.next()?, 16).ok()?;
            Some((start, start + size))
        })
        .collect();
    stacks
        .iter()
        .filter(|stack| {
            stack.functions.iter().any(|name| {
                name.strip_prefix("0x")
                    .and_then(|address| u64::from_str_radix(address, 16).ok())
                    .is_some_and(|address| {
                        functions
                            .iter()
                            .any(|&(start, end)| (start..end).contains(&address))
                    })
            })
        })
        .collect()
}

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
                        "<anonymous>".to_owned()
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
        let checked = path
            .iter()
            .position(|key| function(key) == "report")
            .map_or(path.len(), |report| report + 1);
        for (index, key) in path[..checked].iter().enumerate() {
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

pub fn spinners(output: &str) -> Vec<(String, u32)> {
    Report::parse(output)
        .lines()
        .filter_map(|fields| match fields {
            [kind, native, name] if kind == "spin" => Some((name.clone(), native.parse().ok()?)),
            _ => None,
        })
        .collect()
}

pub fn isolate_lies(
    (spinners, outside): (&[(String, u32)], &[u32]),
    weights: &BTreeMap<u32, u64>,
    zones: &[microscope::profiler::ProfileZone],
) -> Vec<String> {
    let mut lies = Vec::new();
    for tid in outside {
        let labelled: Vec<_> = zones
            .iter()
            .filter(|zone| zone.tid == *tid && zone.name == "isolate")
            .filter_map(|zone| zone.value.as_deref())
            .take(4)
            .collect();
        if !labelled.is_empty() {
            lies.push(format!("{tid}, in no isolate, is labelled {labelled:?}"));
        }
    }
    let mut seen: BTreeMap<String, &str> = BTreeMap::new();
    for (name, tid) in spinners {
        let labels: Vec<_> = zones
            .iter()
            .filter(|zone| zone.tid == *tid && zone.name == "isolate")
            .collect();
        let mut values: Vec<&str> = labels
            .iter()
            .filter_map(|zone| zone.value.as_deref())
            .collect();
        values.sort_unstable();
        values.dedup();
        let [value] = values[..] else {
            let shown: Vec<_> = values.iter().take(4).collect();
            lies.push(format!(
                "{name} ({tid}) is labelled with {} values, as {shown:?}",
                values.len()
            ));
            continue;
        };
        if let Some(other) = seen.insert(value.to_owned(), name) {
            lies.push(format!("{name} ({tid}) is in {other}'s isolate {value}"));
        }
        let labelled: i64 = labels.iter().map(|zone| zone.end - zone.start).sum();
        let sampled = weights.get(tid).copied().unwrap_or(0);
        if u64::try_from(labelled).unwrap_or(0) * 2 < sampled {
            lies.push(format!(
                "{name} ({tid}) is labelled for {labelled} of {sampled} ns"
            ));
        }
    }
    lies
}

#[test]
fn the_isolate_check_fails_on_each_lie() {
    let spinners = [("main".to_owned(), 10), ("worker".to_owned(), 11)];
    let threads = (&spinners[..], &[12][..]);
    let weights = BTreeMap::from([(10, 100), (11, 100), (12, 100)]);
    let zone = |tid: u32, value: &str, end: i64| microscope::profiler::ProfileZone {
        pid: 10,
        tid,
        depth: 0,
        name: "isolate".to_owned(),
        value: Some(value.to_owned()),
        start: 0,
        end,
        flags: 0,
        runtime: true,
    };
    let truth = [zone(10, "1", 100), zone(11, "2", 90)];
    assert!(isolate_lies(threads, &weights, &truth).is_empty());
    for lie in [
        vec![zone(10, "1", 100), zone(11, "2", 100), zone(12, "3", 100)],
        vec![zone(10, "1", 100), zone(11, "1", 100)],
        vec![zone(10, "1", 100)],
        vec![zone(10, "1", 100), zone(11, "2", 50), zone(11, "3", 100)],
        vec![zone(10, "1", 100), zone(11, "2", 40)],
    ] {
        assert_eq!(isolate_lies(threads, &weights, &lie).len(), 1, "{lie:?}");
    }
}

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
        called(&["fast a.js:7", "inner a.js:8"]),
        called(&[
            "spinFor a.js:9",
            "fast",
            "inner a.js:2",
            "report",
            "inner a.js:4",
        ]),
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

pub fn frameless_lies(stacks: &[SampledStack]) -> Vec<String> {
    let spinning: Vec<&SampledStack> = stacks
        .iter()
        .filter(|stack| stack.functions.iter().any(|function| function == "spin"))
        .collect();
    if spinning.len() < 100 {
        return vec![format!("only {} samples in spin", spinning.len())];
    }
    let silent: Vec<&&SampledStack> = spinning
        .iter()
        .filter(|stack| {
            let ends = matches!(&stack.ending, StackEnding::Truncated(reason)
                if reason.starts_with("interpreted frames"));
            !ends
                && !stack
                    .functions
                    .iter()
                    .any(|function| function == "framelessSpins")
        })
        .collect();
    silent
        .first()
        .map(|first| {
            format!(
                "{} of {} samples in spin hide their missing frames, as {first:#?}",
                silent.len(),
                spinning.len()
            )
        })
        .into_iter()
        .collect()
}

#[test]
fn the_frameless_check_fails_on_a_silent_sample() {
    let sample = |functions: &[&str], ending: StackEnding| SampledStack {
        pid: 7,
        tid: 7,
        time: 0,
        weight: 1,
        functions: functions
            .iter()
            .map(|&function| function.to_owned())
            .collect(),
        ending,
        task: None,
    };
    let cut = sample(
        &["spin", "Builtins_JSEntry"],
        StackEnding::Truncated("interpreted frames ended early".into()),
    );
    let named = sample(&["spin", "framelessSpins"], StackEnding::Complete);
    let silent = sample(&["spin", "Builtins_JSEntry"], StackEnding::Complete);
    let mut stacks = vec![cut; 100];
    stacks.push(named);
    assert!(frameless_lies(&stacks).is_empty());
    stacks.push(silent);
    assert_eq!(frameless_lies(&stacks).len(), 1);
    assert_eq!(frameless_lies(&stacks[..99]).len(), 1);
}

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
            .args(["record", "--output"])
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
        let paths = paths(&profile, Source::Samples, Look::Code);
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

    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON: run with `just test-bpf`"]
    fn samples_carry_each_threads_own_isolate() {
        for entry in [ENTRY, "22"] {
            samples_carry_each_threads_own_isolate_on(entry);
        }
    }

    fn samples_carry_each_threads_own_isolate_on(entry: &str) {
        let launcher = std::env::var_os("MICROSCOPE_BPF_LAUNCHER")
            .expect("MICROSCOPE_BPF_LAUNCHER names the microscope-bpf wrapper");
        let scratch = ScratchDir::new("node-busy-isolates");
        let profile = scratch.path().join("busy.usprof");
        let warmed = Command::new(&launcher)
            .arg(env!("CARGO_BIN_EXE_microscope"))
            .args(["record", "--output"])
            .arg(scratch.path().join("warm.usprof"))
            .arg("--")
            .arg(node::node(entry))
            .args(["-e", "0"])
            .env_remove("NODE_OPTIONS")
            .output()
            .expect("run microscope");
        assert!(
            warmed.status.success(),
            "{entry}: {}",
            String::from_utf8_lossy(&warmed.stderr)
        );
        let (fixture, output) = busy_on(entry, &scratch, 5);
        let finished = Command::new(launcher)
            .arg(env!("CARGO_BIN_EXE_microscope"))
            .args(["record", "--output"])
            .arg(&profile)
            .arg("--")
            .arg(fixture.get_program())
            .args(fixture.get_args())
            .arg("--pool")
            .env_remove("NODE_OPTIONS")
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(&output).expect("the fixture's output"))
            .stderr(Stdio::piped())
            .output()
            .expect("run microscope");
        assert!(
            finished.status.success(),
            "{}",
            String::from_utf8_lossy(&finished.stderr)
        );

        let spinners = spinners(&std::fs::read_to_string(&output).expect("the fixture's output"));
        assert_eq!(spinners.len(), 2, "{entry}: {spinners:?}");
        let profile = Profile::read(&profile).expect("read the profile").profile;
        let threads = profile.threads();
        let weights: BTreeMap<u32, u64> = threads
            .iter()
            .map(|thread| (thread.tid, thread.weight))
            .collect();
        let pool: Vec<u32> = threads
            .iter()
            .filter(|thread| {
                let deriving: u64 = process_paths(&profile, vec![thread.row], Look::Native)
                    .iter()
                    .filter(|(path, _)| {
                        path.iter().any(|frame| {
                            let frame = frame.to_ascii_lowercase();
                            frame.contains("pbkdf2") || frame.contains("sha512")
                        })
                    })
                    .map(|(_, weight)| weight)
                    .sum();
                deriving >= 100_000_000
            })
            .map(|thread| thread.tid)
            .collect();
        let sampled: Vec<_> = threads
            .iter()
            .map(|thread| (thread.tid, thread.name.as_deref(), thread.weight))
            .collect();
        assert!(
            !pool.is_empty(),
            "{entry}: no pool thread worked: {sampled:?}"
        );
        let zones = profile.zones();
        let lies = isolate_lies((&spinners, &pool), &weights, &zones);
        assert!(lies.is_empty(), "{entry}: {lies:#?}\n{weights:?}");
    }

    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON: run with `just test-bpf`"]
    fn samples_the_walk_cannot_follow_say_their_javascript_frames_are_missing() {
        let launcher = std::env::var_os("MICROSCOPE_BPF_LAUNCHER")
            .expect("MICROSCOPE_BPF_LAUNCHER names the microscope-bpf wrapper");
        let scratch = ScratchDir::new("node-frameless-bpf");
        let profile = scratch.path().join("frameless.usprof");
        let record = |output: &Path, arguments: &[&std::ffi::OsStr]| {
            let finished = Command::new(&launcher)
                .arg(env!("CARGO_BIN_EXE_microscope"))
                .args(["record", "--output"])
                .arg(output)
                .arg("--")
                .arg(node::node(ENTRY))
                .args(arguments)
                .env_remove("NODE_OPTIONS")
                .stdin(Stdio::null())
                .output()
                .expect("run microscope");
            assert!(
                finished.status.success(),
                "{}",
                String::from_utf8_lossy(&finished.stderr)
            );
        };
        record(
            &scratch.path().join("warm.usprof"),
            &["-e".as_ref(), "0".as_ref()],
        );
        let fixture = node::fixture("frameless.js");
        let addon = crate::support::Scenario::fixture("node-frameless.node");
        record(
            &profile,
            &[
                fixture.as_os_str(),
                addon.as_os_str(),
                "--spin".as_ref(),
                "3".as_ref(),
            ],
        );
        let profile = Profile::read(&profile).expect("read the profile").profile;
        let lies = frameless_lies(&profile.stacks());
        assert!(lies.is_empty(), "{}", lies.join("\n"));
    }

    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON: run with `just test-bpf`"]
    fn a_launched_nodes_first_samples_hold_their_javascript_frames() {
        let launcher = std::env::var_os("MICROSCOPE_BPF_LAUNCHER")
            .expect("MICROSCOPE_BPF_LAUNCHER names the microscope-bpf wrapper");
        let scratch = ScratchDir::new("node-first-bpf");
        let profile = scratch.path().join("first.usprof");
        let finished = Command::new(&launcher)
            .arg(env!("CARGO_BIN_EXE_microscope"))
            .args(["record", "--output"])
            .arg(&profile)
            .arg("--")
            .arg(node::node(ENTRY))
            .args([
                "-e",
                "function spin() { const end = Date.now() + 1500; let n = 0; \
                 while (Date.now() < end) n++; return n; } spin();",
            ])
            .env_remove("NODE_OPTIONS")
            .env("MICROSCOPE_CACHE_DIR", "")
            .stdin(Stdio::null())
            .output()
            .expect("run microscope");
        assert!(
            finished.status.success(),
            "{}",
            String::from_utf8_lossy(&finished.stderr)
        );

        let profile = Profile::read(&profile).expect("read the profile").profile;
        let stacks = profile.stacks();
        let main = stacks.iter().map(|stack| stack.pid).min().expect("samples");
        let ran: Vec<&SampledStack> = stacks.iter().filter(|stack| stack.tid == main).collect();
        let unknown = ran
            .iter()
            .filter(|stack| {
                matches!(&stack.ending, StackEnding::Truncated(reason)
                    if reason == "interpreted frames unknown")
            })
            .count();
        assert!(
            ran.len() >= 500,
            "only {} samples of node's main thread",
            ran.len()
        );
        assert!(
            unknown * 100 <= ran.len(),
            "{unknown} of {} samples of node's main thread have unknown JavaScript frames",
            ran.len()
        );
    }

    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON: run with `just test-bpf`"]
    fn an_attached_nodes_first_samples_hold_their_javascript_frames() {
        use std::io::BufRead as _;
        struct Reaped(std::process::Child);
        impl Drop for Reaped {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        let launcher = std::env::var_os("MICROSCOPE_BPF_LAUNCHER")
            .expect("MICROSCOPE_BPF_LAUNCHER names the microscope-bpf wrapper");
        let scratch = ScratchDir::new("node-attached-bpf");
        let profile = scratch.path().join("attached.usprof");
        let mut spinning = Reaped(
            Command::new(node::node(ENTRY))
                .args([
                    "-e",
                    "console.log('ready'); const end = Date.now() + 60000; let n = 0; \
                     while (Date.now() < end) n++;",
                ])
                .env_remove("NODE_OPTIONS")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .spawn()
                .expect("start node"),
        );
        let mut ready = String::new();
        std::io::BufReader::new(spinning.0.stdout.take().expect("node's output"))
            .read_line(&mut ready)
            .expect("node says it is ready");
        assert_eq!(ready.trim_end(), "ready");

        let finished = Command::new(&launcher)
            .arg(env!("CARGO_BIN_EXE_microscope"))
            .args(["record", "--attach"])
            .arg(spinning.0.id().to_string())
            .args(["--duration", "5"])
            .arg("--output")
            .arg(&profile)
            .env("MICROSCOPE_CACHE_DIR", "")
            .stdin(Stdio::null())
            .output()
            .expect("run microscope");
        assert!(
            finished.status.success(),
            "{}",
            String::from_utf8_lossy(&finished.stderr)
        );

        let main = spinning.0.id();
        let profile = Profile::read(&profile).expect("read the profile").profile;
        let stacks = profile.stacks();
        let ran: Vec<&SampledStack> = stacks.iter().filter(|stack| stack.tid == main).collect();
        let unknown = ran
            .iter()
            .filter(|stack| {
                matches!(&stack.ending, StackEnding::Truncated(reason)
                    if reason == "interpreted frames unknown")
            })
            .count();
        assert!(
            ran.len() >= 500,
            "only {} samples of node's main thread",
            ran.len()
        );
        assert!(
            unknown * 100 <= ran.len(),
            "{unknown} of {} samples of node's main thread have unknown JavaScript frames",
            ran.len()
        );
    }

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
            .args(["record", "--output"])
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
            let paths = process_paths(&profile, rows, Look::Code);
            let truth = spinning(&text);
            assert_eq!(truth.len(), 2, "{pid}: {truth:#?}");
            let functions = profile.keys(Granularity::Function);
            assert_eq!(
                functions.named("<anonymous busy.js:1>").len(),
                1,
                "{pid}: {paths:#?}"
            );
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

    fn process_paths(profile: &Profile, rows: Vec<u32>, look: Look) -> BTreeMap<Vec<String>, u64> {
        let keys = profile.keys(Granularity::Line);
        let tree = keys.aggregate(&Query {
            source: Source::Samples,
            look,
            threads: Some(rows),
            ..Query::default()
        });
        (0..tree.len())
            .filter(|node| tree.own[*node] > 0)
            .map(|node| {
                let mut path = Vec::new();
                let mut at = u32::try_from(node).expect("a node");
                while let Some(&key) = tree.key.get(at as usize) {
                    path.push(reported_name(keys.name(key)));
                    at = tree.parent[at as usize];
                }
                path.reverse();
                (path, tree.own[node])
            })
            .collect()
    }
}

#[test]
fn the_node_package_records_what_the_program_marks() {
    let mut command = Command::new(node::node(ENTRY));
    command
        .arg(node::fixture("probes.js"))
        .env_clear()
        .env("MICROSCOPE_SDK_CHECKS", "1")
        .env(
            "MICROSCOPE_PROBES_LIBRARY",
            crate::support::Scenario::fixture("libmicroscope_probes_c.so"),
        )
        .env(
            "MICROSCOPE_NODE_ADDON",
            crate::support::Scenario::fixture("node-microscope.node"),
        )
        .stdout(Stdio::piped());
    let profile =
        crate::support::released::record_probed(command, &RecordOptions::default()).profile;
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
            .env("MICROSCOPE_SDK_CHECKS", "1")
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
