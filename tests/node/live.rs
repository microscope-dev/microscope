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

/// Whether `rest`, the frames a path holds past those `tiers.js` reported,
/// are those `truth.checkpoint()` parks in: its own, then Node's
/// `fs.readSync`.
fn parked_at_checkpoint(rest: &[String]) -> bool {
    let function = |frame: &String| frame.split(' ').next().unwrap_or_default().to_owned();
    rest.last().is_some_and(|last| function(last) == "readSync")
        && rest[..rest.len() - 1]
            .iter()
            .all(|frame| frame.contains(" truth.js:"))
}

/// A thread parked under a frame of every tier V8 runs, in `tiers.js`, is
/// read live as V8 reported it, function and line, on every build: an
/// interpreted frame, one of Sparkplug's, Maglev's where the build runs
/// Maglev, Turbofan's with a function inlined in it, one that deoptimized,
/// and one that entered optimized code on its loop.
#[test]
fn live_reads_a_frame_of_every_tier_as_v8_reported_it() {
    for entry in crate::session::MATRIX {
        check_tiers(entry);
    }
}

/// Records `tiers.js` on `entry` with the live engine until the thread it
/// parks has been read, and checks that every read found its frames as V8
/// reported them, and none left it out but while it was not asleep.
pub fn check_tiers(entry: &str) {
    let mut command = Command::new(node::node(entry));
    command
        .arg("--allow-natives-syntax")
        .arg(node::fixture("tiers.js"));
    let options = RecordOptions {
        python: Some(PythonEngine::Live { every: EVERY }),
        ..RecordOptions::default()
    };
    let report = |text: &str| {
        Report::parse(text)
            .checkpoint("tiers")
            .map(<[Vec<String>]>::to_vec)
    };
    // From a moment after the checkpoint was reported to the last one
    // before the program was let go.
    let mut window = (None, 0);
    let reads = |profile: &Profile, tid: u64, since: i64| {
        profile
            .task_waits()
            .iter()
            .filter(|wait| wait.noun == "thread" && wait.task == tid && wait.time >= since)
            .count()
    };
    let (profile, lines, text) = record_released(command, &options, report, |profile, lines| {
        // The checkpoint's report is whole once read, and the thread
        // parks after it.
        let now = monotonic();
        let since = *window.0.get_or_insert(now);
        window.1 = now;
        let [thread] = &threads(lines)[..] else {
            return false;
        };
        reads(profile, thread.native, since) >= SETTLED
    });
    let lines = lines.unwrap_or_else(|| panic!("{entry}: no checkpoint:\n{text}"));
    let (since, until) = (window.0.expect("read once reported"), window.1);
    crate::stacks::check_tiers(entry, entry, &Report::parse(&text), &lines);
    let [thread] = &threads(&lines)[..] else {
        panic!("{entry}: one thread");
    };
    let frames = reported_frames(thread);
    let paths = thread_paths(&profile, thread.native, (since, until));
    assert!(!paths.is_empty(), "{entry}: no read found the thread");
    for path in paths.keys() {
        assert!(
            path.starts_with(&frames) && parked_at_checkpoint(&path[frames.len()..]),
            "{entry}: {path:#?}, not {frames:#?}"
        );
    }
    let reasons: Vec<String> = left_out(&profile, (since, until))
        .into_iter()
        .filter(|(tid, _)| *tid == thread.native)
        .map(|(_, reason)| reason)
        .filter(|reason| reason != NOT_ASLEEP)
        .collect();
    assert!(reasons.is_empty(), "{entry}: left out: {reasons:?}");
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

    /// How many reads found a thread calling a site, before the site's
    /// frame began ([`found`]).
    pub calling: usize,
}

/// Where a read found a looping thread, best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Found {
    /// At a site, with the frames V8 reported there.
    Site,
    /// In its phase's call of a site, before the site's frame began.
    Calling,
    /// Anywhere else, which no thread of `live-stress.js` blocks at.
    Wrong,
}

/// Where `path` finds a thread in phase `phase`, whose sites have the
/// paths `sites`, each outermost first.
///
/// A thread can block in its phase's call of a site before the site has a
/// frame: V8 compiles a site whose bytecode it flushed again as it is
/// called, and the compile can wait on a lock, as on the isolate's stack
/// guard (`Runtime_CompileLazy`, `Compiler::Compile`,
/// `StackGuard::PushInterruptsScope`). Its path is then a site's, cut off
/// at the phase's own frame.
fn found(phase: usize, sites: &[Vec<String>], path: &[String]) -> Found {
    if sites.iter().any(|site| site == path) {
        return Found::Site;
    }
    let caller = format!("phase{phase} ");
    let calling = |site: &Vec<String>| {
        site.iter()
            .position(|frame| frame.starts_with(&caller))
            .is_some_and(|at| site[..=at] == *path)
    };
    if sites.iter().any(calling) {
        return Found::Calling;
    }
    Found::Wrong
}

#[test]
fn a_read_finds_a_thread_at_its_sites_or_calling_one() {
    let outer = [" live-stress.js:186", "loop live-stress.js:170"];
    let path = |frames: &[&str]| -> Vec<String> {
        outer
            .iter()
            .chain(frames)
            .map(|&frame| frame.to_owned())
            .collect()
    };
    let phase1 = [
        path(&["phase1 live-stress.js:147", "siteWait live-stress.js:85"]),
        path(&[
            "phase1 live-stress.js:147",
            "siteNested live-stress.js:96",
            "nestedInner live-stress.js:91",
            "readSync fs:741",
        ]),
    ];
    let phase2 = [path(&[
        "phase2 live-stress.js:151",
        "siteWait live-stress.js:85",
    ])];

    assert_eq!(found(1, &phase1, &phase1[1]), Found::Site);
    // As a deoptimizing stress run read it once: a thread of phase 1 that
    // waited on its isolate's stack guard as V8 compiled a flushed site
    // again, before the site had a frame.
    let calling = path(&["phase1 live-stress.js:147"]);
    assert_eq!(found(1, &phase1, &calling), Found::Calling);
    assert_eq!(
        found(2, &phase2, &calling),
        Found::Wrong,
        "another phase's call"
    );
    for wrong in [
        path(&[]),
        path(&["phase1 live-stress.js:148"]),
        path(&[
            "phase1 live-stress.js:147",
            "nestedInner live-stress.js:91",
            "readSync fs:741",
        ]),
        path(&["phase1 live-stress.js:147", "siteNested live-stress.js:96"]),
        path(&["phase1 live-stress.js:147", "siteWait live-stress.js:84"]),
    ] {
        assert_eq!(found(1, &phase1, &wrong), Found::Wrong, "{wrong:?}");
    }
}

pub fn tally(profile: &Profile, looping: &BTreeMap<u64, Looping>) -> Tally {
    let mut tally = Tally::default();
    let waits = profile.task_waits();
    // How long each read that fell behind took, by when it began: a
    // snapshot's waits are stamped when it began, and it reaches each
    // thread before it ends, which on a starved machine can be a second
    // later. One that says nothing ended within the interval.
    let took: BTreeMap<i64, Duration> = profile
        .task_snapshot_problems()
        .iter()
        .filter_map(|(time, problem)| Some((*time, read_took(problem)?)))
        .collect();
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
        // A read finds the thread at a site of a phase it was in while
        // the read ran: the one it was in as the read began, or one it
        // began before the read ended; never of another.
        let times: std::collections::BTreeSet<i64> = waits
            .iter()
            .filter(|wait| wait.noun == "thread" && wait.task == tid)
            .filter(|wait| (since..until).contains(&wait.time))
            .map(|wait| wait.time)
            .collect();
        for &time in &times {
            let ran = took.get(&time).copied().unwrap_or(STRESS_EVERY);
            let ended = time.saturating_add(i64::try_from(ran.as_nanos()).unwrap_or(i64::MAX));
            let current = thread
                .phases
                .iter()
                .rposition(|&(_, began)| began <= time)
                .unwrap_or(0);
            let phases: Vec<usize> = thread.phases[current..]
                .iter()
                .take_while(|&&(_, began)| began <= ended)
                .map(|&(phase, _)| phase)
                .collect();
            for path in thread_paths(profile, tid, (time, time + 1)).keys() {
                let at = phases
                    .iter()
                    .map(|&phase| found(phase, &sites[phase], path))
                    .min()
                    .unwrap_or(Found::Wrong);
                if at == Found::Calling {
                    tally.calling += 1;
                }
                if at == Found::Wrong {
                    tally.wrong.push(format!(
                        "thread {tid} read from {time} to {ended}, in phases {phases:?}: \
                         {path:#?}\nits phases: {:?}",
                        thread.phases
                    ));
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
    // The program waits to loop until the recording reads it, which
    // takes seconds on a loaded machine.
    let waiting = |text: &str| {
        Report::parse(text)
            .lines()
            .any(|fields| fields[0] == "waiting")
            .then_some(())
    };
    let (profile, _, text) = record_released(command, &options, waiting, |profile, ()| {
        profile.live_snapshots().0 > 0
    });
    let looping = looping(&text).unwrap_or_else(|| panic!("{entry} {flags:?}: no report:\n{text}"));
    let tally = tally(&profile, &looping);
    let read = tally.exact + tally.left_out.values().sum::<usize>();
    let (snapshots, changed) = profile.live_snapshots();
    let summary = format!(
        "{entry} {flags:?}: {} exact of {read} reads ({} calling a site), left out {:#?}, \
         {} wrong; {changed} changed while read, in {snapshots} snapshots",
        tally.exact,
        tally.calling,
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
/// `fs.readSync` keep up from the first read on, as each read's wait shows,
/// but for the time the reads that fell behind say they did.
#[test]
fn live_reads_keep_their_interval_or_say_they_fell_behind() {
    let every = Duration::from_millis(1);
    let mut command = Command::new(node::node(ENTRY));
    command.args(["-e", "require('node:fs').readSync(0, Buffer.alloc(1))"]);
    let options = RecordOptions {
        python: Some(PythonEngine::Live { every }),
        ..RecordOptions::default()
    };
    // Reads begin once the program's debug information is loaded, which
    // takes seconds on a loaded machine, so the program waits for them.
    let (profile, _, _) = record_released(
        command,
        &options,
        |_| Some(()),
        |profile, ()| {
            reads_of_one_thread(profile).is_some_and(|(_, first, last)| {
                Duration::from_nanos((last - first).unsigned_abs()) >= READ_FOR
            })
        },
    );
    let Some((reads, first, last)) = reads_of_one_thread(&profile) else {
        panic!("no read found the parked thread");
    };
    let span = Duration::from_nanos((last - first).unsigned_abs());
    // Each read that fell behind says how long it took and how late it
    // began, which accounts for the reads it kept from being taken; the
    // rest of the span must hold reads at no less than half the rate.
    let problems = profile.task_snapshot_problems();
    let notes: Vec<&String> = problems
        .iter()
        .filter(|(time, _)| (first..=last).contains(time))
        .map(|(_, problem)| problem)
        .filter(|problem| problem.starts_with("a live read took"))
        .collect();
    let explained: Duration = notes.iter().filter_map(|note| behind_by(note)).sum();
    assert!(
        notes.iter().all(|note| behind_by(note).is_some()),
        "a note that says not how far behind: {notes:#?}"
    );
    assert!(span >= READ_FOR, "reads spanned {span:?}");
    let unexplained = span.saturating_sub(explained);
    assert!(
        every * u32::try_from(reads).expect("a count") * 2 >= unexplained,
        "{reads} reads in {span:?} at {every:?} each, of which {explained:?} is said to be \
         behind; {:?} snapshots and changed while read in all; problems meanwhile: {:#?}",
        profile.live_snapshots(),
        problems
            .iter()
            .filter(|(time, _)| (first..=last).contains(time))
            .map(|(_, problem)| problem.chars().take(120).collect::<String>())
            .collect::<std::collections::BTreeSet<_>>()
    );
}

/// How long at most a read took, by the note that it fell behind, `a live
/// read took 2.5 ms and began ...`, which rounds to a tenth of a
/// millisecond.
fn read_took(note: &str) -> Option<Duration> {
    let rest = note.strip_prefix("a live read took ")?;
    let (took, _) = rest.split_once(" ms and began ")?;
    Some(Duration::from_secs_f64(
        (took.parse::<f64>().ok()? + 0.05) / 1e3,
    ))
}

/// How far behind a note that a live read fell behind says it was: how
/// long the read took and how late it began, as `a live read took 2.5 ms
/// and began 0.5 ms after it was due, ...`.
fn behind_by(note: &str) -> Option<Duration> {
    let rest = note.strip_prefix("a live read took ")?;
    let (took, rest) = rest.split_once(" ms and began ")?;
    let (late, _) = rest.split_once(" ms after it was due")?;
    let ms = |text: &str| {
        text.parse::<f64>()
            .ok()
            .map(|ms| Duration::from_secs_f64(ms / 1e3))
    };
    Some(ms(took)? + ms(late)?)
}

/// How long the interval test reads its program.
const READ_FOR: Duration = Duration::from_secs(1);

/// How many reads found a thread in `profile`, and when the first and the
/// last began; `None` before any did.
fn reads_of_one_thread(profile: &Profile) -> Option<(u64, i64, i64)> {
    let times: std::collections::BTreeSet<i64> = profile
        .task_waits()
        .iter()
        .filter(|wait| wait.noun == "thread")
        .map(|wait| wait.time)
        .collect();
    Some((
        u64::try_from(times.len()).ok()?,
        *times.first()?,
        *times.last()?,
    ))
}

/// The reason a read gives for a frame in code that none of the mappings
/// it was given covers.
const FOREIGN_CODE: &str =
    "a frame its isolate names runs code that is neither V8's builtins nor code V8 generated";

/// Records `waits.js` on `entry` with the live engine from its start, as
/// its threads block while V8 maps each isolate's code. Every read must
/// find that code, however long the drain before it took to load the debug
/// information of the module that carries V8.
pub fn check_from_start(entry: &str) {
    let mut command = Command::new(node::node(entry));
    command
        .arg(node::fixture("waits.js"))
        .args(["4", "1"])
        .env_clear()
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let options = RecordOptions {
        python: Some(PythonEngine::Live { every: EVERY }),
        ..RecordOptions::default()
    };
    let recording = microscope::profiler::record(command, &options).expect("record");
    assert!(recording.status.success(), "{entry}: {}", recording.status);
    let profile = recording.profile;
    assert!(
        profile.task_snapshots().0 > 0,
        "{entry}: no read: {:#?}",
        profile.task_snapshot_problems()
    );
    let foreign: Vec<u64> = left_out(&profile, (i64::MIN, i64::MAX))
        .into_iter()
        .filter(|(_, reason)| reason == FOREIGN_CODE)
        .map(|(tid, _)| tid)
        .collect();
    assert!(
        foreign.is_empty(),
        "{entry}: left out as foreign code: {foreign:?}"
    );
}
