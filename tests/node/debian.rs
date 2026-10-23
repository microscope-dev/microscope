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

use microscope::ThreadId;

use crate::frames::observed;
use crate::native::check_thread;
use crate::session::NodeSession;
use crate::stacks::check_isolates;
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
