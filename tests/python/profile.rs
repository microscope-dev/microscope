//! Profiles of Python programs: each thread's Python frames, by line, from
//! the engines that read them beside the samples, and which thread holds
//! the GIL.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use microscope::profiler::{
    Granularity, Look, Profile, PythonEngine, Query, RecordError, RecordOptions, Source,
    StackEnding, Watch, record, record_watched,
};

use crate::support::python;

/// The interpreters profiled: before 3.13's table, a GIL build that only
/// names its holder, a free-threaded one with no GIL, and 3.15, whose
/// threads say whether they wait for it.
const PROFILED: [&str; 5] = ["3.12", "3.14", "3.14t", "3.15", "standalone-3.14"];

/// How long a recording holds its program for a profile to be ready
/// before it releases it anyway, and the test fails on what it lacks.
const READY_DEADLINE: Duration = Duration::from_secs(30);

/// The snapshots a recording must hold, past its deadline, before it is
/// released unready, and how long it may wait for them. Each stopped
/// snapshot's debugger reads every library's debug information again,
/// which `just stress`, with no image cache and every CPU busy, can make
/// take most of the deadline; a recording released after one has shown
/// nothing of the program.
const ENOUGH_SNAPSHOTS: u64 = 10;
const SNAPSHOT_DEADLINE: Duration = Duration::from_secs(100);

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

/// How a stack names a frame whose code could not be read.
const UNREADABLE: &str = "[an interpreted frame whose code could not be read]";

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
    ready: impl FnMut(&Profile) -> bool,
) -> Profile {
    record_until_with_args(interpreter, engine, &[], ready)
}

fn record_until_with_args(
    interpreter: &str,
    engine: PythonEngine,
    arguments: &[&str],
    mut ready: impl FnMut(&Profile) -> bool,
) -> Profile {
    let (reader, mut writer) = std::io::pipe().expect("pipe");
    let mut command = Command::new(python::interpreter(interpreter));
    command
        .args(["-I", "-B"])
        .arg(python::fixture("profiled.py"))
        .args(arguments)
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
    let started = Instant::now();
    let recording = record_watched(
        command,
        &options,
        &mut Watch {
            every: Duration::from_millis(50),
            publish: &mut |profile| {
                let waited = started.elapsed();
                let given_up = waited > READY_DEADLINE
                    && (profile.task_snapshots().0 >= ENOUGH_SNAPSHOTS
                        || waited > SNAPSHOT_DEADLINE);
                if !released && (ready(&profile) || given_up) {
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
fn stopped_snapshots_find_each_python_thread_by_line_and_who_holds_the_gil(interpreter: &str) {
    let lines = lines();
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

// One test for each profiled interpreter, since each recording may wait
// long for its snapshots.

#[test]
fn stopped_snapshots_find_each_python_thread_by_line_and_who_holds_the_gil_on_3_12() {
    stopped_snapshots_find_each_python_thread_by_line_and_who_holds_the_gil("3.12");
}

#[test]
fn stopped_snapshots_find_each_python_thread_by_line_and_who_holds_the_gil_on_3_14() {
    stopped_snapshots_find_each_python_thread_by_line_and_who_holds_the_gil("3.14");
}

#[test]
fn stopped_snapshots_find_each_python_thread_by_line_and_who_holds_the_gil_on_3_14t() {
    stopped_snapshots_find_each_python_thread_by_line_and_who_holds_the_gil("3.14t");
}

#[test]
fn stopped_snapshots_find_each_python_thread_by_line_and_who_holds_the_gil_on_3_15() {
    stopped_snapshots_find_each_python_thread_by_line_and_who_holds_the_gil("3.15");
}

#[test]
fn stopped_snapshots_find_each_python_thread_by_line_and_who_holds_the_gil_on_standalone_3_14() {
    stopped_snapshots_find_each_python_thread_by_line_and_who_holds_the_gil("standalone-3.14");
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

#[test]
fn arbitrary_kernel_names_preserve_live_and_stopped_python_profiles() {
    let lines = lines();
    for interpreter in ["3.14", "3.14t"] {
        for engine in [
            PythonEngine::Snapshot { every: EVERY },
            PythonEngine::Live { every: EVERY },
        ] {
            let ready = |profile: &Profile| {
                let paths = python_paths(profile);
                paths.keys().any(|path| spins(path, &lines))
                    && paths.keys().any(|path| naps(path, &lines))
            };

            let profile = record_until_with_args(interpreter, engine, &["--bad-names"], ready);
            assert!(
                ready(&profile),
                "{interpreter}: {:?}",
                python_paths(&profile)
            );

            if matches!(engine, PythonEngine::Snapshot { .. }) {
                assert!(
                    profile.task_snapshot_problems().is_empty(),
                    "{interpreter}: {:?}",
                    profile.task_snapshot_problems()
                );
            } else {
                // Live reads can race thread creation, exit, and mapping
                // records. Require the actual stacks, as the live contract
                // does, rather than treating its visible gaps as failures.
                let (taken, paused, _) = profile.task_snapshots();
                assert!(taken > 0);
                assert_eq!(paused, 0);
            }
        }
    }
}

/// A program instrumented with the `microscope` package is recorded as one
/// instrumented with the Rust SDK is: its zones nest as its blocks and calls
/// did, its labels, plots, and messages land on its thread, and the calls
/// `trace_calls` times are zones named by module and qualified name, a
/// generator's ending as it suspends.
#[test]
fn the_python_package_records_what_the_program_marks() {
    let mut command = Command::new(python::interpreter("3.14"));
    command
        .args(["-I", "-B"])
        .arg(python::fixture("probes.py"))
        .env(
            "MICROSCOPE_PROBES_LIBRARY",
            crate::support::Scenario::fixture("libmicroscope_probes_c.so"),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    let recording = record(command, &RecordOptions::default()).expect("record");
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
            load.start <= parse.start && parse.end <= load.end,
            "{zones:?}"
        );
    }

    let fib = named("__main__.fib");
    let mut depths = fib.iter().map(|zone| zone.depth).collect::<Vec<_>>();
    depths.sort_unstable();
    assert_eq!(depths.len(), 3, "{zones:?}");
    assert!(depths[0] < depths[1] && depths[1] == depths[2], "{zones:?}");
    assert_eq!(named("__main__.numbers").len(), 3, "{zones:?}");
    assert!(named("coroutine").is_empty(), "{zones:?}");

    // Equal code in two modules is two sites, and a prefix added later
    // reaches code that ran before it.
    assert_eq!(named("__main__.traced.work").len(), 1, "{zones:?}");
    assert_eq!(named("elsewhere.work").len(), 1, "{zones:?}");

    let plotted = profile
        .plots()
        .into_iter()
        .map(|plot| (plot.name, plot.value))
        .collect::<Vec<_>>();
    assert_eq!(
        plotted,
        [1.0, 2.0, 3.0].map(|value| ("rows".to_owned(), value))
    );

    let said = profile
        .program_markers()
        .into_iter()
        .filter_map(|marker| marker.text)
        .collect::<Vec<_>>();
    assert_eq!(said, ["round 0", "round 1", "round 2"]);

    // The sites past the table's last record nothing, and the profile
    // says so.
    let problems = profile.probe_problems();
    assert!(
        problems
            .iter()
            .any(|(_, problem)| problem.contains("all 4096 of its run-time sites")),
        "{problems:?}"
    );
}

/// Without its native library the package still imports, and every mark
/// does nothing, as with no recording; `problem` says why.
#[test]
fn the_python_package_without_its_library_does_nothing_and_says_why() {
    let sdk = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("sdk/python");
    let script = r"
import sys
sys.path.insert(0, sys.argv[1])
import microscope
assert 'could not be loaded' in microscope.problem, microscope.problem
@microscope.zone('load')
def load():
    return 1
assert load() == 1 and not microscope.listening()
with microscope.zone('block'), microscope.label('key', 1):
    microscope.plot('rows', 1)
    microscope.message('said', 'text')
    microscope.frame()
    microscope.trigger('why')
microscope.trace_calls('__main__')
assert sys.monitoring.get_tool(sys.monitoring.PROFILER_ID) is None
assert load() == 1
print('NO LIBRARY OK')
";

    let output = Command::new(python::interpreter("3.14"))
        .args(["-I", "-B", "-c", script])
        .arg(sdk)
        .env(
            "MICROSCOPE_PROBES_LIBRARY",
            "/nonexistent/libmicroscope_probes.so",
        )
        .output()
        .expect("run the interpreter");
    let printed = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && printed.contains("NO LIBRARY OK"),
        "{printed}{}",
        String::from_utf8_lossy(&output.stderr)
    );
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

/// The BPF engine refuses an unsupported recorder namespace or missing
/// privileges, naming the requirement and the engines that need none,
/// and the program never starts.
#[test]
fn the_bpf_engine_names_the_missing_requirement_and_starts_nothing() {
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

    let initial = std::fs::read_link("/proc/self/ns/pid")
        .is_ok_and(|link| link == std::path::Path::new("pid:[4026531836]"));
    assert!(
        why.contains(if initial {
            "needs CAP_BPF and CAP_PERFMON"
        } else {
            "initial PID namespace"
        }),
        "{why}"
    );
    assert!(why.contains("--python snapshot"), "{why}");
    assert!(!marker.exists(), "the program started");
}

/// Perf records use the recorder's PID namespace; BPF currently reports
/// initial-namespace IDs. Refuse the mismatch before starting the target.
#[test]
fn a_namespaced_bpf_recorder_refuses_unproven_kernel_keys() {
    struct Reaped(std::process::Child);
    impl Drop for Reaped {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let scratch = crate::support::ScratchDir::new("bpf-recorder-namespace");
    let marker = scratch.path().join("started");
    let output = scratch.path().join("stderr");
    let errors = std::fs::File::create(&output).expect("record stderr");

    let mut child = Reaped(
        Command::new("unshare")
            .args([
                "--user",
                "--map-root-user",
                "--pid",
                "--fork",
                "--kill-child=KILL",
                "--mount-proc",
            ])
            .arg(env!("CARGO_BIN_EXE_microscope"))
            .args(["record", "--python", "bpf", "--output"])
            .arg(scratch.path().join("capture.usprof"))
            .arg("--")
            .arg(python::interpreter("3.14"))
            .args([
                "-I",
                "-c",
                "import pathlib, sys; pathlib.Path(sys.argv[1]).touch()",
            ])
            .arg(&marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(errors)
            .spawn()
            .expect("start namespaced recorder"),
    );

    let deadline = Instant::now() + READY_DEADLINE;

    let status = loop {
        if let Some(status) = child.0.try_wait().expect("poll recorder") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "namespaced recorder did not finish"
        );
        std::thread::sleep(Duration::from_millis(10));
    };

    let error = std::fs::read_to_string(output).expect("read recorder error");
    assert!(
        !error.trim_start().starts_with("unshare:"),
        "namespace test setup needs unprivileged user, PID and mount namespaces: {error}"
    );
    assert!(!status.success(), "the mismatched engine started");
    assert!(error.contains("initial PID namespace"), "{error}");
    assert!(error.contains("--python snapshot"), "{error}");
    assert!(!marker.exists(), "the target started");
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
        recorded_with(interpreter, "profiled.py", &[], running)
    }

    /// `fixture`'s profile under `interpreter` by the BPF engine, run with
    /// `environment`, from a recording that lets the program go after
    /// `running`.
    fn recorded_with(
        interpreter: &str,
        fixture: &str,
        environment: &[(&str, &std::ffi::OsStr)],
        running: Duration,
    ) -> Profile {
        // The recording runs in a process of its own, which the test cannot
        // watch for samples as it records: it runs for `running`.
        let scratch = crate::support::ScratchDir::new("python-bpf");
        let output = scratch
            .path()
            .join(format!("{interpreter}-{fixture}.usprof"));

        let mut child = Command::new(launcher())
            .arg(env!("CARGO_BIN_EXE_microscope"))
            .arg("record")
            .arg("--python")
            .arg("bpf")
            .arg("--output")
            .arg(&output)
            .arg("--")
            .arg(python::interpreter(interpreter))
            .args(if environment.is_empty() {
                ["-I", "-B"].as_slice()
            } else {
                ["-s", "-B"].as_slice()
            })
            .arg(python::fixture(fixture))
            .envs(environment.iter().copied())
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

        Profile::read(&output).expect("read the profile").profile
    }

    /// A thread that never enters the interpreter's loop, as a library's
    /// worker thread does, runs no Python frames: its samples are whole,
    /// not cut short by frames that cannot be known.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON: run with `just test-bpf`"]
    fn threads_that_run_no_python_have_no_unknown_frames() {
        let site = python::interpreter("3.14").with_file_name("site");
        let environment = [
            ("PYTHONPATH", site.as_os_str()),
            ("OMP_NUM_THREADS", std::ffi::OsStr::new("2")),
        ];

        recorded_with(
            "3.14",
            "workers.py",
            &environment,
            Duration::from_millis(200),
        );

        let profile = recorded_with("3.14", "workers.py", &environment, Duration::from_secs(1));
        let stacks = profile.stacks();
        let workers = stacks
            .iter()
            .filter(|stack| {
                stack
                    .functions
                    .iter()
                    .any(|function| function == "gomp_thread_start")
            })
            .collect::<Vec<_>>();
        assert!(!workers.is_empty(), "no samples on workers: {stacks:#?}");

        let unknown = workers
            .iter()
            .filter(|stack| {
                stack.ending == StackEnding::Truncated("interpreted frames unknown".into())
            })
            .count();

        assert_eq!(
            unknown,
            0,
            "{unknown} of {} workers' samples",
            workers.len()
        );
    }

    /// A program that replaces itself with another interpreter is read by
    /// the new one's modules: the spinners' samples of the program it
    /// runs hold their Python frames by line.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON: run with `just test-bpf`"]
    fn a_program_an_exec_replaces_is_read_by_its_own_interpreter() {
        let lines = lines();
        let scratch = crate::support::ScratchDir::new("python-bpf-exec");
        let output = scratch.path().join("exec.usprof");

        let mut child = Command::new(launcher())
            .arg(env!("CARGO_BIN_EXE_microscope"))
            .args(["record", "--python", "bpf", "--output"])
            .arg(&output)
            .arg("--")
            .arg(python::interpreter("3.12"))
            .args(["-I", "-B"])
            .arg(python::fixture("exec_profiled.py"))
            .arg(python::interpreter("3.14"))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run microscope");

        std::thread::sleep(Duration::from_secs(3));

        child
            .stdin
            .take()
            .expect("its input")
            .write_all(b"\n")
            .expect("release the program");

        let finished = child.wait_with_output().expect("microscope finishes");
        assert!(
            finished.status.success(),
            "{}",
            String::from_utf8_lossy(&finished.stderr)
        );

        let profile = Profile::read(&output).expect("read the profile").profile;
        let python = paths(&profile, Source::Samples, Look::Python);
        let spinning: u64 = python
            .iter()
            .filter(|(path, _)| spins(path, &lines))
            .map(|(_, weight)| weight)
            .sum();

        assert!(spinning > 0, "no sample through work: {python:#?}");
    }

    /// Each CPU sample of a spinner holds its Python frames by line,
    /// spliced before the activations of the interpreter's loop that run
    /// them: the Python look is `spin` calling `work`, with no frame of the
    /// interpreter's, and the mixed stacks keep both. A GIL build's
    /// spinners hold its GIL in turn; a free-threaded build's is off. Only
    /// the samples taken before the engine knew the program are unknown,
    /// and only those of its last moments, after the drain before it
    /// exited, have code that could not be read.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON: run with `just test-bpf`"]
    fn samples_hold_each_threads_python_frames_by_line() {
        let lines = lines();
        for interpreter in PROFILED {
            // The engine names no Python frames until microscope has read
            // the interpreter's image, which a cold image cache can make
            // a tenth of the recording. A first recording fills it.
            recorded(interpreter, Duration::from_millis(200));

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
            // A stack that ends short, its Python frames unknown or its
            // native ones cut short, shows its native frames.
            for path in python.keys() {
                assert!(
                    path.first().is_some_and(|end| end.starts_with('['))
                        || !path.iter().any(|step| evaluates(step)),
                    "{interpreter}: the Python look shows the interpreter: {path:?}"
                );
            }

            let stacks = profile.stacks();
            let mixed = stacks.iter().any(|stack| {
                stack.functions.iter().any(|function| function == "work")
                    && stack.functions.iter().any(|function| evaluates(function))
            });
            assert!(mixed, "{interpreter}: no mixed stack of work");

            // Each count is of samples, as each stack is one.
            let samples = stacks.len();
            let ending = |reason: &str| {
                stacks
                    .iter()
                    .filter(|stack| stack.ending == StackEnding::Truncated(reason.into()))
                    .count()
            };

            let unknown = ending("interpreted frames unknown");
            assert!(
                unknown * 10 < samples,
                "{interpreter}: {unknown} of {samples} samples have unknown frames"
            );

            // A sample can catch the loop as it begins or ends a frame,
            // so a few may not place theirs; most place every segment.
            let unplaced = ending("interpreted frames not placed among the native ones");
            assert!(
                unplaced * 10 < samples,
                "{interpreter}: {unplaced} of {samples} samples do not place their frames"
            );

            // GCC's epilogues pop the registers a caller keeps but leave
            // the rules that name their slots, below a sample's copy of
            // the stack; a sample there still reaches its caller.
            let cut = ending("invalid caller");
            assert!(
                cut * 100 < samples,
                "{interpreter}: {cut} of {samples} samples have an invalid caller"
            );

            let unreadable = stacks
                .iter()
                .filter(|stack| {
                    stack
                        .functions
                        .iter()
                        .any(|function| function == UNREADABLE)
                })
                .count();

            assert!(
                unreadable * 200 < samples,
                "{interpreter}: {unreadable} of {samples} samples have frames that could not be read"
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
