//! Profiles of Python programs: each thread's Python frames, by line, from
//! the engines that read them beside the samples, and which thread holds
//! the GIL.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use microscope::profiler::{
    Granularity, Look, Profile, PythonEngine, Query, RecordOptions, Source, Watch, record_watched,
};

use crate::support::python;

/// The interpreters profiled: before 3.13's table, a GIL build that only
/// names its holder, a free-threaded one with no GIL, and 3.15, whose
/// threads say whether they wait for it.
const PROFILED: [&str; 4] = ["3.12", "3.14", "3.14t", "3.15"];

/// How long a recording holds its program for a profile to be ready
/// before it releases it anyway, and the test fails on what it lacks.
const READY_DEADLINE: Duration = Duration::from_secs(30);

/// How often each engine reads the program.
const EVERY: Duration = Duration::from_millis(20);

/// The fixture's marked lines, by their marks.
fn lines() -> BTreeMap<String, u32> {
    let source = std::fs::read_to_string(python::fixture("profiled.py")).expect("read the fixture");
    source
        .lines()
        .zip(1..)
        .filter_map(|(text, number)| {
            let (_, mark) = text.split_once("# LINE ")?;
            Some((mark.trim().to_owned(), number))
        })
        .collect()
}

/// Each path of the waits' call tree that waits end at, outermost first,
/// as the Python look shows them by line, with the weight that ends there.
fn python_paths(profile: &Profile) -> BTreeMap<Vec<String>, u64> {
    let keys = profile.keys(Granularity::Line);
    let tree = keys.aggregate(&Query {
        source: Source::Waits,
        look: Look::Python,
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

/// Whether a Python path ends in the fixture's `work`, called from `spin`.
fn spins(path: &[String], lines: &BTreeMap<String, u32>) -> bool {
    let [.., outer, inner] = path else {
        return false;
    };

    let work = ["work-loop", "work-add"].map(|mark| format!("work profiled.py:{}", lines[mark]));
    *outer == format!("spin profiled.py:{}", lines["spin"]) && work.contains(inner)
}

/// Whether a Python path passes through the fixture's `nap`.
fn naps(path: &[String], lines: &BTreeMap<String, u32>) -> bool {
    let nap = format!("nap profiled.py:{}", lines["nap"]);
    path.contains(&nap)
}

/// Whether a function is the interpreter's loop: the switch it is
/// compiled as, or one of the functions a tail-calling build makes of it.
fn evaluates(function: &str) -> bool {
    function.starts_with("_PyEval_EvalFrame") || function.starts_with("_TAIL_CALL_")
}

/// The values the GIL's label spans have.
fn gil_values(profile: &Profile) -> Vec<String> {
    let mut values: Vec<String> = profile
        .zones()
        .into_iter()
        .filter(|zone| zone.name == "gil")
        .filter_map(|zone| zone.value)
        .collect();
    values.sort();
    values.dedup();
    values
}

/// Records the fixture under `interpreter` with `engine`, holding it until
/// a profile so far is `ready`, or the deadline, then releasing it.
fn record_until(
    interpreter: &str,
    engine: PythonEngine,
    mut ready: impl FnMut(&Profile) -> bool,
) -> Profile {
    let (reader, mut writer) = std::io::pipe().expect("pipe");
    let mut command = Command::new(python::interpreter(interpreter));
    command
        .args(["-I", "-B"])
        .arg(python::fixture("profiled.py"))
        .stdin(reader)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let options = RecordOptions {
        python: Some(engine),
        debug_directories: python::debug_directory(interpreter).into_iter().collect(),
        ..RecordOptions::default()
    };

    let stop = AtomicBool::new(false);
    let mut released = false;
    let deadline = Instant::now() + READY_DEADLINE;
    let recording = record_watched(
        command,
        &options,
        &mut Watch {
            every: Duration::from_millis(50),
            publish: &mut |profile| {
                if !released && (ready(&profile) || Instant::now() > deadline) {
                    writer.write_all(b"\n").expect("release the program");
                    released = true;
                }
            },
            stop: &stop,
            save: None,
        },
    )
    .expect("record");
    assert!(
        recording.status.success(),
        "{interpreter}: {}",
        recording.status
    );
    recording.profile
}

/// Snapshots that stop the program find each thread where it is, its
/// Python frames among the native frames that run them: by line, the Python
/// look is the spinners' `spin` calling `work` and the napper's `nap`, with
/// no frame of the interpreter's. A GIL build's spinners hold its GIL in
/// turn, and from 3.15 one waits for it as the other holds it; a
/// free-threaded build's GIL is off.
#[test]
fn stopped_snapshots_find_each_python_thread_by_line_and_who_holds_the_gil() {
    let lines = lines();
    for interpreter in PROFILED {
        let free_threaded = interpreter.ends_with('t');
        let wanted: &[&str] = match interpreter {
            _ if free_threaded => &[],
            "3.15" => &["held", "waiting"],
            _ => &["held"],
        };

        let ready = |profile: &Profile| {
            let paths = python_paths(profile);
            paths.keys().any(|path| spins(path, &lines))
                && paths.keys().any(|path| naps(path, &lines))
                && gil_values(profile) == wanted
        };

        let profile = record_until(interpreter, PythonEngine::Snapshot { every: EVERY }, ready);
        let (taken, paused, _) = profile.task_snapshots();
        assert!(taken > 0 && paused > 0, "{interpreter}: {taken} {paused}");
        let problems = profile.task_snapshot_problems();
        assert!(problems.is_empty(), "{interpreter}: {problems:?}");

        let paths = python_paths(&profile);
        assert!(
            paths.keys().any(|path| spins(path, &lines)),
            "{interpreter}: no spinner in {paths:#?}"
        );
        assert!(
            paths.keys().any(|path| naps(path, &lines)),
            "{interpreter}: no napper in {paths:#?}"
        );
        for path in paths.keys() {
            assert!(
                !path.iter().any(|step| evaluates(step)),
                "{interpreter}: the Python look shows the interpreter: {path:?}"
            );
        }

        // The mixed stacks hold the interpreter's frames that run them.
        let mixed = profile.task_waits().into_iter().any(|wait| {
            wait.noun == "thread"
                && wait.functions.iter().any(|function| function == "work")
                && wait.functions.iter().any(|function| evaluates(function))
        });
        assert!(mixed, "{interpreter}: no mixed stack of work");
        assert_eq!(gil_values(&profile), wanted, "{interpreter}");
    }
}

/// Live snapshots read each thread's Python frames as the program runs,
/// never stopping it: only Python frames, each thread whose frames changed
/// as they were read left out and counted, never kept shorter.
#[test]
fn live_snapshots_read_each_python_thread_as_it_runs() {
    let lines = lines();
    for interpreter in PROFILED {
        let ready = |profile: &Profile| {
            let paths = python_paths(profile);
            paths.keys().any(|path| spins(path, &lines))
                && paths.keys().any(|path| naps(path, &lines))
        };

        let profile = record_until(interpreter, PythonEngine::Live { every: EVERY }, ready);
        let (taken, paused, _) = profile.task_snapshots();
        let (live, _) = profile.live_snapshots();
        assert!(taken > 0, "{interpreter}");
        assert_eq!(
            (live, paused),
            (taken, 0),
            "{interpreter}: it never stops the program"
        );

        let paths = python_paths(&profile);
        assert!(
            paths.keys().any(|path| spins(path, &lines)),
            "{interpreter}: no spinner in {paths:#?}"
        );
        assert!(
            paths.keys().any(|path| naps(path, &lines)),
            "{interpreter}: no napper in {paths:#?}"
        );

        // Every frame is the program's Python code or the library's.
        for wait in profile.task_waits() {
            assert_eq!(wait.noun, "thread");
            assert!(
                !wait.functions.iter().any(|function| evaluates(function)),
                "{interpreter}: a live stack with native frames: {wait:?}"
            );
        }
    }
}
