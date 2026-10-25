//! Live snapshots of Node programs, which never stop the program: each
//! thread blocked in native code that JavaScript called is read from what
//! V8 publishes in its isolate, frame by frame and line by line as V8's own
//! call sites reported it (`tests/fixtures/node/live.js`); an idle thread
//! has no frames, and one that runs JavaScript is left out, by its reason.

use std::collections::BTreeMap;
use std::process::Command;
use std::time::Duration;

use microscope::profiler::{Look, Profile, PythonEngine, Query, RecordOptions, Source};

use crate::profile::{ENTRY, line_key, paths_of, record_released};
use crate::support::truth::Report;
use crate::support::{Scenario, ScratchDir, node};
use crate::truth::{Thread, threads};

/// The checkpoints of the threads `live.js` blocks, each named by how.
const BLOCKED: [&str; 10] = [
    "reviver", "proxy", "sort", "nested", "execfile", "atomics", "napi", "fast", "control",
    "debugger",
];

/// How often the live engine reads the program: its default.
const EVERY: Duration = Duration::from_millis(10);

/// How many live snapshots, after every thread reported where it blocks,
/// a recording holds before it is checked.
const SETTLED: usize = 20;

/// Why a thread is left out that was in no system call when read.
const NOT_ASLEEP: &str = "it was not asleep in a system call";

/// Why a thread is left out whose frames include WebAssembly's.
const WEBASSEMBLY: &str = "it runs WebAssembly, whose frames a live read does not walk";

/// What `live.js` reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reported {
    /// The blocked threads, in [`BLOCKED`]'s order.
    pub blocked: Vec<Thread>,

    /// The idle, spinning, and WebAssembly workers' native ids, and when
    /// they began.
    pub idle: (u64, i64),
    pub spin: (u64, i64),
    pub wasm: (u64, i64),

    /// How many calls took the fast path, and the line of the fast call.
    pub fast: (u64, u32),
}

/// What the output `text` reported, once every thread has.
pub fn reported(text: &str) -> Option<Reported> {
    let report = Report::parse(text);
    let blocked = BLOCKED
        .iter()
        .map(|name| {
            let mut found = threads(report.checkpoint(name)?);
            (found.len() == 1).then(|| found.remove(0))
        })
        .collect::<Option<Vec<_>>>()?;
    Some(Reported {
        blocked,
        idle: pair(&report, "idle")?,
        spin: pair(&report, "spin")?,
        wasm: pair(&report, "wasm")?,
        fast: pair(&report, "fast")?,
    })
}

/// The two numbers of the first line of `kind` in `report`.
fn pair<A: std::str::FromStr, B: std::str::FromStr>(report: &Report, kind: &str) -> Option<(A, B)> {
    let fields = report.lines().find(|fields| fields[0] == kind)?;
    Some((fields.get(1)?.parse().ok()?, fields.get(2)?.parse().ok()?))
}

/// Now, on the clock the profile's times are on.
fn monotonic() -> i64 {
    let now = nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC).expect("the clock");
    now.tv_sec() * 1_000_000_000 + now.tv_nsec()
}

/// Whether `entry` runs V8 12.4, whose Turbofan code keeps no line for the
/// frame that makes a fast API call.
fn fast_call_unlined(entry: &str) -> bool {
    entry.ends_with("22")
}

/// The paths of thread `tid`'s waits in `[since, until)`.
fn thread_paths(
    profile: &Profile,
    tid: u64,
    (since, until): (i64, i64),
) -> BTreeMap<Vec<String>, u64> {
    let rows: Vec<u32> = profile
        .tasks()
        .iter()
        .filter(|task| task.noun == "thread" && task.number == tid)
        .map(|task| task.row)
        .collect();
    if rows.is_empty() {
        return BTreeMap::new();
    }
    paths_of(
        profile,
        &Query {
            source: Source::Waits,
            look: Look::Python,
            tasks: Some(rows),
            range: Some((since, until)),
            ..Query::default()
        },
    )
}

/// Each thread the problems in `[since, until)` leave out, by its reason: a
/// live snapshot's problem names them as `frames not read live: threads
/// 12, 13: REASON; threads 14: REASON`.
fn left_out(profile: &Profile, (since, until): (i64, i64)) -> Vec<(u64, String)> {
    let mut left = Vec::new();
    for (time, problem) in profile.task_snapshot_problems() {
        let Some(groups) = problem.strip_prefix("frames not read live: threads ") else {
            continue;
        };
        if time < since || time >= until {
            continue;
        }
        // The snapshot's other problems, which it counts after its first.
        let groups = match groups.rsplit_once(", and ") {
            Some((groups, more)) if more.ends_with(" more") => groups,
            _ => groups,
        };
        for group in groups.split("; threads ") {
            let (threads, reason) = group.split_once(": ").expect("threads and a reason");
            for tid in threads.split(", ") {
                left.push((tid.parse().expect("a thread id"), reason.to_owned()));
            }
        }
    }
    left
}

/// The builtins `live.js` calls that V8 writes in C++, which run in an exit
/// frame, which no snapshot shows as a function; V8's builtins in its own
/// generated code, as `Object.keys` and `Array.prototype.sort`, run in a
/// frame of their own, which a snapshot names, without a line.
const CPP_BUILTINS: [&str; 1] = ["parse"];

/// The frames, outermost first, that every path of a blocked thread's
/// waits begins with, as V8 reported them. A fast call's caller, which runs
/// in a frame of its own, is no call site of the line that called it, and
/// is added.
fn expected(reported: &Reported, name: &str, thread: &Thread) -> Vec<String> {
    let mut frames = reported_frames(thread);
    if name == "fast" {
        frames.push(format!("fastCaller live.js:{}", reported.fast.1));
    }
    frames
}

/// The frames, outermost first, V8 reported of `thread`, as a profile's
/// interpreted look names them: a builtin of V8's generated code by its
/// name alone, and one of its C++ builtins not at all.
fn reported_frames(thread: &Thread) -> Vec<String> {
    thread
        .frames
        .iter()
        .rev()
        .filter(|frame| frame.file != "-" || !CPP_BUILTINS.contains(&frame.function.as_str()))
        .map(|frame| {
            if frame.file == "-" && frame.line.is_none() {
                frame.function.clone()
            } else {
                line_key(&frame.function, &frame.file, frame.line)
            }
        })
        .collect()
}

/// Whether `rest`, the frames a blocked thread's path holds past those V8
/// reported, are the ones the native call of `name` runs: Node's
/// `fs.readSync` for those that read, Node's own frames for `execFileSync`,
/// and none for the others, which call native code straight from the
/// fixture.
fn native_call(name: &str, rest: &[String]) -> bool {
    let function = |frame: &String| frame.split(' ').next().unwrap_or_default().to_owned();
    match name {
        "reviver" | "proxy" | "sort" | "nested" | "control" => {
            rest.iter().map(function).eq(["readSync"])
        }
        "execfile" => {
            !rest.is_empty()
                && rest
                    .iter()
                    .all(|frame| !frame.contains(" live.js:") && !frame.contains(" truth.js:"))
        }
        _ => rest.is_empty(),
    }
}

/// Every way the profile differs from what `live.js` reported on `entry`,
/// read in `[since, until)`: from a moment after every thread reported to
/// one before the program was let go. A blocked thread may be left out
/// while it is not asleep, as one that just blocked, or that V8 woke to
/// serve an interrupt, is not; for no other reason.
pub fn lies(
    entry: &str,
    profile: &Profile,
    reported: &Reported,
    window: (i64, i64),
) -> Vec<String> {
    let (since, until) = window;
    let mut lies = Vec::new();
    let left = left_out(profile, window);
    let left_out_threads = |tid: u64| -> Vec<&str> {
        left.iter()
            .filter(|(thread, _)| *thread == tid)
            .map(|(_, reason)| reason.as_str())
            .collect()
    };
    for (name, thread) in BLOCKED.iter().zip(&reported.blocked) {
        let paths = thread_paths(profile, thread.native, window);
        let reasons = left_out_threads(thread.native);
        if *name == "fast" && fast_call_unlined(entry) {
            if !paths.is_empty() || reasons.is_empty() {
                lies.push(format!(
                    "{name}: a fast call's caller has no line on {entry}, so the thread is left \
                     out, not {paths:#?} {reasons:?}"
                ));
            }
            continue;
        }
        let frames = expected(reported, name, thread);
        if paths.is_empty() {
            lies.push(format!(
                "{name}: no waits, but {reasons:?}; expected {frames:#?}"
            ));
        }
        for path in paths.keys() {
            let exact = path.starts_with(&frames) && native_call(name, &path[frames.len()..]);
            if !exact {
                lies.push(format!("{name}: {path:#?}, not {frames:#?}"));
            }
        }
        if reasons.iter().any(|reason| *reason != NOT_ASLEEP) {
            lies.push(format!("{name}: left out: {reasons:?}"));
        }
    }
    if reported.fast.0 == 0 {
        lies.push("no call took the fast path".to_owned());
    }

    let (idle, idle_since) = reported.idle;
    let paths = thread_paths(profile, idle, (idle_since.max(since), until));
    let reasons = left_out_threads(idle);
    if !paths.is_empty() || !reasons.is_empty() {
        lies.push(format!("idle: {paths:#?} {reasons:?}"));
    }
    let (spin, spin_since) = reported.spin;
    let paths = thread_paths(profile, spin, (spin_since.max(since), until));
    let reasons = left_out_threads(spin);
    if !paths.is_empty() || reasons.is_empty() || reasons.iter().any(|reason| *reason != NOT_ASLEEP)
    {
        lies.push(format!("spin: {paths:#?} {reasons:?}"));
    }
    let (wasm, wasm_since) = reported.wasm;
    let paths = thread_paths(profile, wasm, (wasm_since.max(since), until));
    let reasons = left_out_threads(wasm);
    if !paths.is_empty()
        || !reasons.contains(&WEBASSEMBLY)
        || reasons
            .iter()
            .any(|reason| *reason != WEBASSEMBLY && *reason != NOT_ASLEEP)
    {
        lies.push(format!("wasm: {paths:#?} {reasons:?}"));
    }
    lies
}

/// Records `live.js` on `entry` with the live engine until it has read
/// every thread, and checks it: no lie, and nothing the fixture said
/// differs once it exited.
pub fn check(entry: &str) {
    let scratch = ScratchDir::new("node-live");
    let fifo = |name: &str| {
        let path = scratch.path().join(name);
        nix::unistd::mkfifo(
            &path,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .expect("make a FIFO");
        path
    };
    let mut command = Command::new(node::node(entry));
    command
        .arg("--allow-natives-syntax")
        .arg(node::fixture("live.js"))
        .arg(fifo("quiet"))
        .arg(fifo("opened"))
        .arg(Scenario::fixture("node-addon.node"))
        .arg(Scenario::fixture("node-frameless.node"))
        .arg(Scenario::fixture(&format!("node-fast-{entry}.node")));
    let options = RecordOptions {
        python: Some(PythonEngine::Live { every: EVERY }),
        ..RecordOptions::default()
    };

    let mut window = (None, 0);
    let (profile, used, text) =
        record_released(command, &options, reported, |profile, reported| {
            let now = monotonic();
            let since = *window.0.get_or_insert(now);
            window.1 = now;
            let (spin, _) = reported.spin;
            let reads = left_out(profile, (since, now))
                .iter()
                .filter(|(tid, _)| *tid == spin)
                .count();
            reads >= SETTLED && lies(entry, profile, reported, (since, now)).is_empty()
        });
    let used = used.unwrap_or_else(|| panic!("{entry}: the fixture reported nothing:\n{text}"));
    let window = (window.0.expect("read once reported"), window.1);
    assert_eq!(
        Some(&used),
        reported(&text).as_ref(),
        "{entry}: the report changed"
    );
    let lies = lies(entry, &profile, &used, window);
    assert!(
        lies.is_empty(),
        "{entry}:\n{}\n{:#?}",
        lies.join("\n"),
        profile.task_snapshot_problems().last()
    );
}

#[test]
fn live_snapshots_read_each_blocked_thread_on_26() {
    check("26");
}

#[test]
fn live_snapshots_read_each_blocked_thread_on_nixpkgs_26() {
    check("nixpkgs-26");
}

#[test]
fn live_snapshots_read_each_blocked_thread_on_24() {
    check("24");
}

#[test]
fn live_snapshots_read_each_blocked_thread_on_nixpkgs_24() {
    check("nixpkgs-24");
}

#[test]
fn live_snapshots_read_each_blocked_thread_on_22() {
    check("22");
}

#[test]
fn live_snapshots_read_each_blocked_thread_on_nixpkgs_22() {
    check("nixpkgs-22");
}

/// The sites `live-stress.js` blocks at.
const STRESS_SITES: [&str; 6] = ["wait", "nested", "reviver", "sort", "fast", "mixed"];

/// How many phases `live-stress.js` cycles through, each calling the sites
/// through a function of its own.
const STRESS_PHASES: usize = 4;

/// The workers `live-stress.js` starts beside its main thread, and how
/// long each loops.
const STRESS_WORKERS: usize = 3;
const STRESS_SECONDS: u32 = 4;

/// How often the stress recordings read the program.
const STRESS_EVERY: Duration = Duration::from_millis(2);

/// The least share of a stress recording's reads of a looping thread that
/// must find it at a site: the rest are left out, by reason, mostly as
/// not asleep or as changed while read. A thread blocks for about a
/// millisecond at a time.
const LEAST_EXACT: f64 = 0.25;

/// What `live-stress.js` reported of one thread: each phase's frames at
/// each site, when it ran only the sites, and when it began each phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Looping {
    /// By phase, then by site.
    pub sites: Vec<Vec<(String, Thread)>>,
    pub window: (i64, i64),
    /// Each phase the thread began, with when, in order.
    pub phases: Vec<(usize, i64)>,
}

/// What `live-stress.js` reported of each thread, by native id, once every
/// thread is done.
pub fn looping(text: &str) -> Option<BTreeMap<u64, Looping>> {
    let report = Report::parse(text);
    let times = |kind: &str| -> BTreeMap<u64, i64> {
        report
            .lines()
            .filter(|fields| fields[0] == kind)
            .filter_map(|fields| Some((fields.get(1)?.parse().ok()?, fields.get(2)?.parse().ok()?)))
            .collect()
    };
    let (ready, done) = (times("ready"), times("done"));
    if done.len() != STRESS_WORKERS + 1 {
        return None;
    }
    done.iter()
        .map(|(&tid, &end)| {
            let sites = (0..STRESS_PHASES)
                .map(|phase| {
                    STRESS_SITES
                        .iter()
                        .map(|site| {
                            let name = format!("{site} {phase} {tid}");
                            let mut found = threads(report.checkpoint(&name)?);
                            (found.len() == 1).then(|| ((*site).to_owned(), found.remove(0)))
                        })
                        .collect::<Option<Vec<_>>>()
                })
                .collect::<Option<Vec<_>>>()?;
            let phases = report
                .lines()
                .filter(|fields| fields[0] == "phase" && fields.get(1) == Some(&tid.to_string()))
                .map(|fields| Some((fields.get(2)?.parse().ok()?, fields.get(3)?.parse().ok()?)))
                .collect::<Option<Vec<(usize, i64)>>>()?;
            Some((
                tid,
                Looping {
                    sites,
                    window: (*ready.get(&tid)?, end),
                    phases,
                },
            ))
        })
        .collect()
}

/// What the live reads of the threads `looping` names found in `profile`:
/// how many found each thread at a site, how many left it out, by reason,
/// and each path that is at no site of its thread.
#[derive(Debug, Default)]
pub struct Tally {
    pub exact: usize,
    pub left_out: BTreeMap<String, usize>,
    pub wrong: Vec<String>,

    /// Each read that found a thread twice, or both found it and left it
    /// out.
    pub twice: Vec<String>,
}

pub fn tally(profile: &Profile, looping: &BTreeMap<u64, Looping>) -> Tally {
    let mut tally = Tally::default();
    let waits = profile.task_waits();
    for (&tid, thread) in looping {
        let (since, until) = thread.window;
        let sites: Vec<Vec<Vec<String>>> = thread
            .sites
            .iter()
            .map(|phase| {
                phase
                    .iter()
                    .map(|(_, reported)| reported_frames(reported))
                    .collect()
            })
            .collect();
        // A read that began in a phase finds the thread at a site of that
        // phase, or of the next, which it may have begun before the read
        // reached it; never of another.
        for (index, &(phase, began)) in thread.phases.iter().enumerate() {
            let ended = thread.phases.get(index + 1).map_or(i64::MAX, |&(_, at)| at);
            let span = (began.max(since), ended.min(until));
            if span.0 >= span.1 {
                continue;
            }
            let next = (phase + 1) % STRESS_PHASES;
            for path in thread_paths(profile, tid, span).keys() {
                if !sites[phase].contains(path) && !sites[next].contains(path) {
                    tally
                        .wrong
                        .push(format!("thread {tid} in phase {phase}: {path:#?}"));
                }
            }
        }
        let found: Vec<i64> = waits
            .iter()
            .filter(|wait| wait.noun == "thread" && wait.task == tid)
            .filter(|wait| (since..until).contains(&wait.time))
            .map(|wait| wait.time)
            .collect();
        tally.exact += found.len();
        let mut reads = std::collections::BTreeSet::new();
        for &time in &found {
            if !reads.insert(time) {
                tally
                    .twice
                    .push(format!("thread {tid} found twice at {time}"));
            }
        }
        for (time, problem) in profile.task_snapshot_problems() {
            if reads.contains(&time) && problem.contains(&format!(" {tid}")) {
                tally
                    .twice
                    .push(format!("thread {tid} found at {time}, and {problem}"));
            }
        }
        for (left, reason) in left_out(profile, thread.window) {
            if left == tid {
                *tally.left_out.entry(reason).or_default() += 1;
            }
        }
    }
    tally
}

/// Records `live-stress.js` on `entry` with V8's `flags`, reading it live
/// every few milliseconds: no read finds a thread anywhere but at one of
/// its sites, and enough find it there.
pub fn stress(entry: &str, flags: &[&str]) {
    let mut command = Command::new(node::node(entry));
    command
        .arg("--allow-natives-syntax")
        .args(flags)
        .arg(node::fixture("live-stress.js"))
        .arg(Scenario::fixture("node-addon.node"))
        .arg(Scenario::fixture(&format!("node-fast-{entry}.node")))
        .arg(STRESS_WORKERS.to_string())
        .arg(STRESS_SECONDS.to_string());
    let options = RecordOptions {
        python: Some(PythonEngine::Live {
            every: STRESS_EVERY,
        }),
        ..RecordOptions::default()
    };
    let (profile, looping, text) = record_released(command, &options, looping, |_, _| true);
    let looping = looping.unwrap_or_else(|| panic!("{entry} {flags:?}: no report:\n{text}"));
    let tally = tally(&profile, &looping);
    let read = tally.exact + tally.left_out.values().sum::<usize>();
    let (snapshots, changed) = profile.live_snapshots();
    let summary = format!(
        "{entry} {flags:?}: {} exact of {read} reads, left out {:#?}, {} wrong; \
         {changed} changed while read, in {snapshots} snapshots",
        tally.exact,
        tally.left_out,
        tally.wrong.len()
    );
    eprintln!("{summary}");
    // A recording whose reads fell behind what it asked says so.
    let behind = profile
        .task_snapshot_problems()
        .iter()
        .filter(|(_, problem)| problem.starts_with("a live read took"))
        .count();
    let asked = Duration::from_secs(STRESS_SECONDS.into()).as_nanos() / STRESS_EVERY.as_nanos();
    assert!(
        behind > 0 || u128::from(snapshots) * 2 >= asked,
        "{summary}: {snapshots} snapshots of {asked} asked, and none said it fell behind"
    );
    assert!(
        tally.wrong.is_empty(),
        "{summary}\n{}",
        tally.wrong.join("\n")
    );
    assert!(
        tally.twice.is_empty(),
        "{summary}\n{:#?}",
        &tally.twice[..tally.twice.len().min(5)]
    );
    // A read that found its thread had run meanwhile is of no thread the
    // profile names: each counts against every thread's share, as if it
    // were a read of this program's looping threads.
    #[expect(clippy::cast_precision_loss, reason = "counts of reads")]
    let share = tally.exact as f64 / (read as f64 + changed as f64).max(1.0);
    assert!(share >= LEAST_EXACT, "{summary}");
}

/// V8's flags that make it collect garbage every few thousand
/// allocations, and move every object it keeps as it does.
const COLLECTING: &[&str] = &["--stress-compaction", "--gc-interval=2000"];

/// V8's flags that make it deoptimize optimized code often, and flush
/// code and bytecode, which it compiles again.
const DEOPTIMIZING: &[&str] = &[
    "--deopt-every-n-times=40",
    "--stress-flush-code",
    "--flush-bytecode",
];

#[test]
fn live_reads_under_collection_find_each_thread_at_its_site_on_26() {
    stress("26", COLLECTING);
}

#[test]
fn live_reads_under_deoptimization_find_each_thread_at_its_site_on_26() {
    stress("26", DEOPTIMIZING);
}

#[test]
fn live_reads_under_collection_find_each_thread_at_its_site_on_22() {
    stress("22", COLLECTING);
}

#[test]
fn live_reads_under_deoptimization_find_each_thread_at_its_site_on_22() {
    stress("22", DEOPTIMIZING);
}

/// A live recording reads as often as it is asked to, or says it fell
/// behind: reads at a millisecond's interval of a program parked in
/// `Atomics.wait` either keep up from the first read on, as each read's
/// wait shows, or each say how long they took.
#[test]
fn live_reads_keep_their_interval_or_say_they_fell_behind() {
    let every = Duration::from_millis(1);
    let mut command = Command::new(node::node(ENTRY));
    command.args([
        "-e",
        "Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 5000)",
    ]);
    let options = RecordOptions {
        python: Some(PythonEngine::Live { every }),
        ..RecordOptions::default()
    };
    let (profile, _, _) = record_released(command, &options, |_| Some(()), |_, ()| true);
    // Reads begin once the program's debug information is loaded, which
    // takes longer on a loaded machine.
    let reads: std::collections::BTreeSet<i64> = profile
        .task_waits()
        .iter()
        .filter(|wait| wait.noun == "thread")
        .map(|wait| wait.time)
        .collect();
    let (Some(&first), Some(&last)) = (reads.first(), reads.last()) else {
        panic!("no read found the parked thread");
    };
    let asked = u64::try_from((last - first) / 1_000_000).expect("a span");
    let behind = profile
        .task_snapshot_problems()
        .iter()
        .filter(|(time, problem)| {
            (first..=last).contains(time) && problem.starts_with("a live read took")
        })
        .count();
    let reads = u64::try_from(reads.len()).expect("a count");
    assert!(
        asked >= 500,
        "reads spanned {asked} ms of the program's 5 s"
    );
    assert!(
        reads * 2 >= asked || u64::try_from(behind).expect("a count") * 2 >= reads,
        "{reads} reads in {asked} ms at 1 ms each, {behind} said they fell behind"
    );
}
