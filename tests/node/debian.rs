//! Debian's node, whose shared libnode is stripped of the symbols that
//! name V8's builtins and of their call-frame information: the debugger
//! finds the builtins where the process says V8 keeps them, and unwinds
//! them by V8's convention.
//!
//! Debian's node runs only inside the mount namespace its `enter` makes
//! (flake.nix), so each test runs itself again there, as uid 0 of a user
//! namespace, with the harness and the debugger: the outer run waits for
//! the inner one, which debugs, and fails with its output.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use microscope::{BreakpointOptions, BreakpointSpec, LineNumber, StopReason, ThreadId};

use crate::frames::observed;
use crate::native::check_thread;
use crate::session::NodeSession;
use crate::stacks::check_isolates;
use crate::support::{ExternalProcess, Scenario, ScratchDir, node};
use crate::truth::{check_threads, threads};

/// The build, as the flake names it.
const DEBIAN: &str = "debian-24";

/// Set in the run inside the namespace.
const INSIDE: &str = "MICROSCOPE_NODE_DEBIAN_INSIDE";

/// How long the run inside the namespace may take.
const DEADLINE: Duration = Duration::from_secs(60);

/// Whether this is the run inside the namespace; if not, runs the test
/// `name` there and fails as it does.
fn inside(name: &str) -> bool {
    if std::env::var_os(INSIDE).is_some() {
        return true;
    }
    let enter = crate::support::node::node(DEBIAN).with_file_name("enter");
    let mut child = Command::new(enter)
        .arg(std::env::current_exe().expect("the test binary"))
        .args([name, "--exact", "--nocapture", "--test-threads", "1"])
        .env(INSIDE, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("enter Debian's namespace");
    // The output is read as it is written, so that a full pipe never
    // stops the run.
    let mut stdout = child.stdout.take().expect("the run's output");
    let mut stderr = child.stderr.take().expect("the run's errors");
    let out = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let err = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });

    let began = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("the run's status") {
            break Some(status);
        }
        if began.elapsed() > DEADLINE {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let output = format!(
        "{}\n{}",
        out.join().unwrap_or_default(),
        err.join().unwrap_or_default()
    );
    match status {
        None => panic!("{name} ran past {DEADLINE:?} inside Debian's namespace:\n{output}"),
        Some(status) => assert!(
            status.success() && output.contains("1 passed"),
            "{name} inside Debian's namespace: {status}:\n{output}"
        ),
    }
    false
}

/// Each thread `stacks.js` parks is labelled with its isolate, which a
/// symbol Debian strips names the variable of, and has the interpreted
/// frames V8 reported, though no symbol names the builtin that runs them,
/// and unwinds natively
/// through V8's builtins, and their copies, to where it began.
#[tokio::test]
async fn debian_parked_threads_are_the_ones_v8_reported_and_unwind_to_where_they_began() {
    if !inside(
        "debian::debian_parked_threads_are_the_ones_v8_reported_and_unwind_to_where_they_began",
    ) {
        return;
    }
    let mut session = NodeSession::launch(DEBIAN, "stacks.js", &[], &[]).await;
    let reported = threads(&session.stop_at("parked").await);
    let context = session.context();
    let snapshot = session.stopped().await;
    check_isolates(&context, &reported, &snapshot);
    let seen = observed(&session.scenario, &reported).await;
    let lie = check_threads(&seen, &reported).err();

    let scenario = &session.scenario;
    let modules = scenario
        .operation("modules", scenario.handle().loaded_modules())
        .await;
    for thread in &reported {
        let context = format!("{context} thread {}", thread.name);
        scenario
            .operation(
                "select",
                scenario
                    .handle()
                    .select_context(ThreadId::new(thread.native)),
            )
            .await;
        let trace = scenario
            .operation("backtrace", scenario.handle().backtrace())
            .await;
        let (_, interpreted) = check_thread(&context, thread.printer, &trace, &modules);
        assert!(interpreted > 0, "{context}: no interpreted frame");
    }
    session.shutdown().await;
    assert!(lie.is_none(), "{context}: {lie:?}");
}

/// The stack of `tiers.js` unwinds through the builtins and the code each
/// compiler made to `_start`, and its frames of every tier, inlined ones
/// included, are the ones V8 reported. Debian's build has no symbol for the
/// JS dispatch table, so it is found from V8's exported code.
#[tokio::test]
async fn debian_frames_of_every_tier_are_the_ones_v8_reported() {
    if !inside("debian::debian_frames_of_every_tier_are_the_ones_v8_reported") {
        return;
    }
    let mut session =
        NodeSession::launch(DEBIAN, "tiers.js", &["--allow-natives-syntax"], &[]).await;
    let mut reported = threads(&session.stop_at("tiers").await);
    let context = session.context();
    let [thread] = &reported[..] else {
        panic!("{context}: one thread: {reported:#?}");
    };
    let native = thread.native;
    let scenario = &session.scenario;
    let modules = scenario
        .operation("modules", scenario.handle().loaded_modules())
        .await;
    scenario
        .operation(
            "select",
            scenario.handle().select_context(ThreadId::new(native)),
        )
        .await;
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    let (compiled, _) = check_thread(&context, true, &trace, &modules);
    // A backtrace does not report tiers, so compare frames without them.
    for frame in reported.iter_mut().flat_map(|thread| &mut thread.frames) {
        frame.tier = None;
    }
    let seen = observed(&session.scenario, &reported).await;
    let lie = check_threads(&seen, &reported).err();
    session.shutdown().await;
    assert!(compiled >= 2, "{context}: {compiled} compiled frames");
    assert!(lie.is_none(), "{context}: {lie:?}");
}

/// Debian's libnode names `GetTargetIds` only in its dynamic table, where
/// microscope finds it to read and hide the inspector's address.
#[tokio::test]
async fn debian_launched_nodes_inspector_is_shown_to_no_one() {
    if !inside("debian::debian_launched_nodes_inspector_is_shown_to_no_one") {
        return;
    }
    crate::inspector::check_hidden(DEBIAN).await;
}

/// On Debian's node, every kind of breakpoint stops, logs, and waits as
/// asked, and each exception stops where V8 pauses at it, with V8's frames
/// and locals at every stop; a worker stops
/// and steps on its own thread; and an attached node stops too.
#[tokio::test]
async fn debian_breakpoints_and_exceptions_match_v8() {
    if !inside("debian::debian_breakpoints_and_exceptions_match_v8") {
        return;
    }
    crate::breakpoints::every_kind_of_breakpoint(DEBIAN).await;
    let mut lies = crate::exceptions::lies(DEBIAN).await;
    lies.extend(crate::workers::worker_lies(DEBIAN).await);
    lies.extend(crate::attach::attach_lies(DEBIAN).await);
    assert!(lies.is_empty(), "{}", lies.join("\n"));
}

/// Main-thread stepping has its own namespace deadline, rather than sharing
/// one with four independent run-control scenarios under cold-cache stress.
#[tokio::test]
async fn debian_steps_match_v8() {
    if !inside("debian::debian_steps_match_v8") {
        return;
    }
    let lies = crate::steps::lies(DEBIAN).await;
    assert!(lies.is_empty(), "{}", lies.join("\n"));
}

#[tokio::test]
async fn debian_assignments_reach_the_program() {
    if !inside("debian::debian_assignments_reach_the_program") {
        return;
    }
    crate::assignments::each_binding_takes_each_value(DEBIAN).await;
}

#[tokio::test]
async fn debian_watches_are_refused_without_v8_s_collector() {
    if !inside("debian::debian_watches_are_refused_without_v8_s_collector") {
        return;
    }
    crate::watches::watches_are_refused_without_the_collector(DEBIAN).await;
}

#[tokio::test]
async fn debian_detach_closes_its_owned_inspector_and_preserves_a_borrowed_one() {
    if !inside("debian::debian_detach_closes_its_owned_inspector_and_preserves_a_borrowed_one") {
        return;
    }

    for borrowed in [false, true] {
        let scratch = ScratchDir::new("debian-inspector-close");
        let report = scratch.path().join("report");
        let mut command = Command::new(node::node(DEBIAN));
        command.arg("--inspect-port=0");
        if borrowed {
            command.arg("--inspect=127.0.0.1:0");
        }
        let mut child = ExternalProcess::spawn_command(
            command
                .arg(node::fixture("attach-detach.js"))
                .arg(&report)
                .env_clear(),
        );
        let debugger = child.attach().await;
        let mut scenario =
            Scenario::attached(format!("Debian inspector borrowed={borrowed}"), debugger);
        scenario
            .operation(
                "add breakpoint",
                scenario.handle().add_breakpoint_with(
                    BreakpointSpec::Source {
                        path: node::fixture("attach-detach.js"),
                        line: LineNumber::new(9).expect("a line"),
                    },
                    BreakpointOptions {
                        pending: true,
                        ..BreakpointOptions::default()
                    },
                ),
            )
            .await;
        let running = scenario.start_resuming().await;
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let snapshot = scenario.snapshot().await;
            if !snapshot.breakpoints[0].locations.is_empty()
                && snapshot.breakpoints[0].pending_locations.is_empty()
            {
                break;
            }
            assert!(Instant::now() < deadline, "Debian breakpoint never arms");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        child.release();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(20), running)
                .await
                .expect("breakpoint deadline")
                .expect("run task")
                .expect("run"),
            StopReason::Breakpoint { .. }
        ));
        let before: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&report).expect("before report")).expect("JSON");
        let address: std::net::SocketAddr = before["before"]
            .as_str()
            .expect("open inspector")
            .strip_prefix("ws://")
            .expect("URL")
            .split('/')
            .next()
            .expect("authority")
            .parse()
            .expect("address");
        scenario.shutdown().await;
        assert_eq!(
            std::net::TcpStream::connect_timeout(&address, Duration::from_secs(1)).is_ok(),
            borrowed,
            "Debian inspector listener after detach"
        );
        child.release();
        let deadline = Instant::now() + Duration::from_secs(5);
        let after = loop {
            let value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&report).expect("live report"))
                    .unwrap_or(serde_json::Value::Null);
            if value.get("after").is_some() {
                break value;
            }
            assert!(
                Instant::now() < deadline,
                "Debian detached process cannot progress"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(
            after["after"],
            if borrowed {
                before["before"].clone()
            } else {
                serde_json::Value::Null
            }
        );
        assert!(child.wait().success(), "Debian detached process survives");
    }
}

/// At every checkpoint of `awaits.js`, each thread's await chain is the
/// one V8 reported: the builtins that resume an await, which no symbol
/// names in Debian's build, have the numbers of its V8 release's.
#[tokio::test]
async fn debian_await_chains_are_the_ones_v8_reported() {
    if !inside("debian::debian_await_chains_are_the_ones_v8_reported") {
        return;
    }
    let mut session = NodeSession::launch(DEBIAN, "awaits.js", &[], &[]).await;
    let mut lies = Vec::new();
    for (index, (checkpoint, resumed, ends_early)) in
        crate::awaits::CHECKPOINTS.into_iter().enumerate()
    {
        if index > 0 {
            session.proceed().await;
        }
        let reported = threads(&session.stop_at(checkpoint).await);
        let context = format!("{} at {checkpoint}", session.context());
        lies.extend(
            crate::awaits::check(&session.scenario, &reported, resumed, ends_early, &context).await,
        );
    }
    session.shutdown().await;
    assert!(lies.is_empty(), "{lies:#?}");
}

/// Debian's event loop work is what its fixture reported: its handles,
/// requests, and timers, and a named gap for the pool its shared libuv
/// keeps.
#[tokio::test]
async fn debian_tasks_are_the_ones_node_reported() {
    if !inside("debian::debian_tasks_are_the_ones_node_reported") {
        return;
    }
    if let Err(lies) = crate::tasks::task_lies(DEBIAN).await {
        panic!("{lies}");
    }
}

/// Debian's V8 reserves its code range below `libnode`, so an attach, which
/// maps what the process holds in address order, meets the range before it
/// knows the process carries V8. The range is V8's code all the same once
/// `libnode` is mapped: samples in it unwind on into `libnode`, as a
/// launched recording's do, rather than end at an unmapped address.
#[test]
fn debian_attached_samples_unwind_through_v8s_generated_code() {
    struct Killed(std::process::Child);
    impl Drop for Killed {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    // Samples whose frame in V8's code a frame in `libnode` called.
    const ENOUGH: usize = 10;
    const SPINNING: Duration = Duration::from_secs(15);
    const RECORDING: Duration = Duration::from_secs(30);
    if !inside("debian::debian_attached_samples_unwind_through_v8s_generated_code") {
        return;
    }
    let through = |profile: &microscope::profiler::Profile| {
        profile
            .stacks()
            .iter()
            .filter(|stack| {
                stack
                    .functions
                    .windows(2)
                    .any(|pair| pair[0].starts_with("0x") && pair[1].starts_with("libnode.so"))
            })
            .count()
    };

    let scratch = ScratchDir::new("node-debian-attach");
    let (mut command, output) = crate::profile::busy_on(DEBIAN, &scratch, 60);
    let child = Killed(command.spawn().expect("start busy.js"));
    let began = Instant::now();
    while !std::fs::read_to_string(&output)
        .unwrap_or_default()
        .contains("TRUTH\tspin\t")
    {
        assert!(began.elapsed() < SPINNING, "busy.js never spun");
        std::thread::sleep(Duration::from_millis(10));
    }

    let stop = std::sync::atomic::AtomicBool::new(false);
    let recording = Instant::now();
    let attachment = microscope::profiler::record_attached(
        child.0.id(),
        &microscope::profiler::RecordOptions::default(),
        &mut microscope::profiler::Watch {
            every: Duration::from_millis(100),
            publish: &mut |profile| {
                if through(&profile) >= ENOUGH || recording.elapsed() > RECORDING {
                    stop.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            },
            stop: &stop,
            save: None,
        },
    )
    .expect("attach");
    drop(child);

    let profile = attachment.profile;
    let through = through(&profile);
    assert!(
        through >= ENOUGH,
        "{through} of {} samples unwound from V8's code into libnode; their ends: {:?}",
        profile.stacks().len(),
        profile.truncations()
    );
}

/// On Debian's node, whose libnode keeps each thread's isolate in dynamic
/// thread-local storage, live snapshots read each thread blocked in native
/// code as V8 reported it, and leave out the one that runs JavaScript.
#[test]
fn debian_live_snapshots_read_each_blocked_thread() {
    if !inside("debian::debian_live_snapshots_read_each_blocked_thread") {
        return;
    }
    crate::live::check(DEBIAN);
}
