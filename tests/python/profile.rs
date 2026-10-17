//! Profiles of Python programs: each thread's Python frames, by line, from
//! the engines that read them beside the samples, and which thread holds
//! the GIL.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use microscope::profiler::{
    Granularity, Look, Profile, PythonEngine, Query, RecordError, RecordOptions, Source, Watch,
    record, record_watched,
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
    paths(profile, Source::Waits, Look::Python)
}

/// Each path of `source`'s call tree in `look` that weight ends at,
/// outermost first, by line, with the weight that ends there.
fn paths(profile: &Profile, source: Source, look: Look) -> BTreeMap<Vec<String>, u64> {
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

/// Whether a Python path passes through `spin` calling `work`. Native
/// callees of `work` remain in the Python look and may follow those frames.
fn spins(path: &[String], lines: &BTreeMap<String, u32>) -> bool {
    let spin = format!("spin profiled.py:{}", lines["spin"]);
    let work = ["work-loop", "work-add"].map(|mark| format!("work profiled.py:{}", lines[mark]));
    path.windows(2).enumerate().any(|(at, pair)| {
        pair[0] == spin
            && work.contains(&pair[1])
            && path[at + 2..].iter().all(|frame| !frame.contains(".py:"))
    })
}

/// Reproduce a snapshot taken inside a native callee of Python's `work`.
/// The Python look deliberately retains native callees after Python frames.
#[test]
fn spinner_detection_keeps_native_callees_and_rejects_wrong_callers() {
    let lines = lines();
    let spin = format!("spin profiled.py:{}", lines["spin"]);
    let work = format!("work profiled.py:{}", lines["work-add"]);
    let native = "_PyLong_FromMedium longobject.c:210".to_owned();
    assert!(spins(&[spin.clone(), work.clone()], &lines));
    assert!(spins(&[spin.clone(), work.clone(), native.clone()], &lines));
    for path in [
        vec![
            spin.clone(),
            work.clone(),
            format!("nap profiled.py:{}", lines["nap"]),
        ],
        vec![spin.clone(), work.clone(), work.clone()],
        vec![
            spin.clone(),
            work.clone(),
            "helper threading.py:12".to_owned(),
        ],
        vec![work.clone(), spin.clone(), native.clone()],
        vec![work.clone(), native.clone()],
        vec![spin.clone(), "other.py:1".to_owned(), work],
        vec![spin, "work profiled.py:1".to_owned(), native],
    ] {
        assert!(!spins(&path, &lines), "{path:?}");
    }
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

/// Whether this process holds `CAP_BPF` and `CAP_PERFMON`, as the BPF
/// engine needs.
fn privileged() -> bool {
    const CAP_SYS_ADMIN: u32 = 21;
    const CAP_PERFMON: u32 = 38;
    const CAP_BPF: u32 = 39;
    let status = std::fs::read_to_string("/proc/self/status").expect("read the status");
    let effective = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .and_then(|bits| u64::from_str_radix(bits.trim(), 16).ok())
        .expect("CapEff");
    let has = |capability: u32| effective & (1 << capability) != 0;
    has(CAP_SYS_ADMIN) || (has(CAP_BPF) && has(CAP_PERFMON))
}

/// The BPF engine never falls back to another: without its privileges
/// it refuses, naming the one it lacks and the engines that need none,
/// and the program never starts.
#[test]
fn the_bpf_engine_names_the_privilege_it_lacks_and_starts_nothing() {
    if privileged() {
        // `just test-bpf` runs the engine itself (`bpf` below).
        return;
    }

    let marker =
        std::env::temp_dir().join(format!("microscope-bpf-refused-{}", std::process::id()));
    let mut command = Command::new(python::interpreter("3.14"));
    command
        .args([
            "-I",
            "-c",
            "import pathlib, sys; pathlib.Path(sys.argv[1]).touch()",
        ])
        .arg(&marker);
    let options = RecordOptions {
        python: Some(PythonEngine::Bpf),
        ..RecordOptions::default()
    };

    let Err(RecordError::Python(why)) = record(command, &options) else {
        panic!("the BPF engine ran without its privileges");
    };

    assert!(why.contains("needs CAP_BPF and CAP_PERFMON"), "{why}");
    assert!(why.contains("--python snapshot"), "{why}");
    assert!(!marker.exists(), "the program started");
}

/// The BPF engine, which reads each sampled thread's Python frames in the
/// kernel as the sample is taken. It needs `CAP_BPF` and `CAP_PERFMON`,
/// so these run only through `just test-bpf`, which records through the
/// `microscope-bpf` wrapper.
mod bpf {
    use std::path::PathBuf;

    use super::*;

    fn launcher() -> PathBuf {
        std::env::var_os("MICROSCOPE_BPF_LAUNCHER").map_or_else(
            || {
                panic!(
                    "MICROSCOPE_BPF_LAUNCHER names no launcher: run these with `just test-bpf`, \
                     which records through the microscope-bpf wrapper"
                )
            },
            PathBuf::from,
        )
    }

    /// The fixture's profile under `interpreter` by the BPF engine, from
    /// a recording that lets the program go after `running`.
    fn recorded(interpreter: &str, running: Duration) -> Profile {
        let output = std::env::temp_dir().join(format!(
            "microscope-bpf-{}-{interpreter}.usprof",
            std::process::id()
        ));
        let mut child = Command::new(launcher())
            .arg(env!("CARGO_BIN_EXE_microscope"))
            .arg("record")
            .arg("--python")
            .arg("bpf")
            .arg("--output")
            .arg(&output)
            .arg("--")
            .arg(python::interpreter(interpreter))
            .args(["-I", "-B"])
            .arg(python::fixture("profiled.py"))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run microscope");
        std::thread::sleep(running);
        child
            .stdin
            .take()
            .expect("its input")
            .write_all(b"\n")
            .expect("release the program");
        let finished = child.wait_with_output().expect("microscope finishes");
        assert!(
            finished.status.success(),
            "{interpreter}: {}",
            String::from_utf8_lossy(&finished.stderr)
        );
        let profile = Profile::read(&output).expect("read the profile").profile;
        let _ = std::fs::remove_file(&output);
        profile
    }

    /// Each CPU sample of a spinner holds its Python frames by line,
    /// spliced before the activations of the interpreter's loop that run
    /// them: the Python look is `spin` calling `work`, with no frame of the
    /// interpreter's, and the mixed stacks keep both. A GIL build's
    /// spinners hold its GIL in turn; a free-threaded build's is off. Only
    /// the samples taken before the engine knew the program are unknown.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON: run with `just test-bpf`"]
    fn samples_hold_each_threads_python_frames_by_line() {
        let lines = lines();
        for interpreter in PROFILED {
            let profile = recorded(interpreter, Duration::from_secs(2));
            let python = paths(&profile, Source::Samples, Look::Python);
            let spinning: u64 = python
                .iter()
                .filter(|(path, _)| spins(path, &lines))
                .map(|(_, weight)| weight)
                .sum();
            let total: u64 = python.values().sum();
            assert!(
                spinning * 2 > total,
                "{interpreter}: {spinning} of {total} through work: {python:#?}"
            );
            for path in python.keys() {
                assert!(
                    !path.iter().any(|step| evaluates(step)),
                    "{interpreter}: the Python look shows the interpreter: {path:?}"
                );
            }

            let mixed = profile.stacks().into_iter().any(|stack| {
                stack.functions.iter().any(|function| function == "work")
                    && stack.functions.iter().any(|function| evaluates(function))
            });
            assert!(mixed, "{interpreter}: no mixed stack of work");

            let unknown = profile
                .truncations()
                .into_iter()
                .find(|(reason, _)| reason == "interpreted frames unknown")
                .map_or(0, |(_, count)| count);
            assert!(
                unknown * 10 < total,
                "{interpreter}: {unknown} of {total} samples have unknown frames"
            );

            let gil = gil_values(&profile);
            if interpreter.ends_with('t') {
                assert_eq!(gil, Vec::<String>::new(), "{interpreter}");
            } else {
                assert!(gil.contains(&"held".to_owned()), "{interpreter}: {gil:?}");
            }
        }
    }
}
