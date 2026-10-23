//! Node fixtures parked under the debugger, and in core dumps, where they
//! said they would be. microscope reads no JavaScript frames yet: these
//! hold the fixtures and the harness to their contract, and the debugger to
//! the threads the fixtures name.

use std::collections::BTreeMap;

use microscope::{CoreDumpOptions, InferiorState, StateSnapshot};

use crate::session::{CORES, MATRIX, NodeSession, major};
use crate::support::Scenario;
use crate::support::truth::Report;
use crate::truth::{Thread, functions_in, threads, unavailable};

/// Each thread of `stacks.js`: its name, its functions from the innermost
/// out, and the value it names.
const STACKS: [(&str, &[&str], (&str, &str)); 3] = [
    (
        "main",
        &["tally", "settle", "mainOuter", "-"],
        ("total", "6"),
    ),
    (
        "alpha",
        &["counterStep", "sortAll", "alphaStart", "-"],
        ("count", "3"),
    ),
    (
        "beta",
        &["betaInnermost", "betaHandled", "betaFetched"],
        ("label", "'beta-7'"),
    ),
];

/// Holds what `stacks.js` reported to the threads, frames, and values the
/// fixture parks with.
fn check_stacks(context: &str, reported: &[Thread]) {
    assert_eq!(reported.len(), STACKS.len(), "{context}: {reported:#?}");
    for (thread, (name, functions, (value, text))) in reported.iter().zip(STACKS) {
        let context = format!("{context} thread {name}: {thread:#?}");
        assert_eq!(thread.name, name, "{context}");
        assert_eq!(thread.printer, name == "main", "{context}");
        assert_eq!(functions_in(thread, "stacks.js"), functions, "{context}");
        assert_eq!(thread.frames[0].function, functions[0], "{context}");
        let values: Vec<(&str, &str)> = thread
            .values
            .iter()
            .map(|value| (value.name.as_str(), value.text.as_str()))
            .collect();
        assert_eq!(values, [(value, text)], "{context}");
    }
}

/// Holds what `tiers.js` reported on the build `entry` to a frame of each
/// tier that build can put on the stack, and the rest reported unavailable.
fn check_tiers(context: &str, entry: &str, report: &Report, lines: &[Vec<String>]) {
    // Node 22's V8 runs no Maglev code unless told to, so its OSR is to
    // Turbofan and its maglev frame stays interpreted.
    let maglev = major(entry) >= 24;
    let expected: [(&str, Option<&str>); 8] = [
        ("ignition", Some("ignition")),
        (
            "osr",
            Some(if maglev {
                "ignition>maglev"
            } else {
                "ignition>turbofan"
            }),
        ),
        ("deoptimized", Some("turbofan>ignition")),
        ("sparkplug", Some("sparkplug")),
        ("maglev", Some(if maglev { "maglev" } else { "ignition" })),
        ("inlined", Some("inlined")),
        ("turbofan", Some("turbofan")),
        ("-", None),
    ];

    let reported = threads(lines);
    let [thread] = &reported[..] else {
        panic!("{context}: one thread: {reported:#?}");
    };
    let frames: Vec<_> = thread
        .frames
        .iter()
        .filter(|frame| frame.file.ends_with("/tests/fixtures/node/tiers.js"))
        .map(|frame| (frame.function.as_str(), frame.tier.as_deref()))
        .collect();
    assert_eq!(frames, expected, "{context}: {thread:#?}");

    let missing: &[&str] = if maglev { &[] } else { &["maglev"] };
    assert_eq!(unavailable(report), missing, "{context}");
}

/// Every thread `reported` names is one the debugger lists at the stop.
fn check_listed(context: &str, reported: &[Thread], listed: &[u64]) {
    for thread in reported {
        assert!(
            listed.contains(&thread.native),
            "{context}: thread {} ({}) is not among {listed:?}",
            thread.native,
            thread.name
        );
    }
}

/// The isolate each thread `reported`, which runs JavaScript, is labelled
/// with at the stop `snapshot` lists, by thread id: each its own number,
/// the main thread's 1. Any other thread has no label, or the label of the
/// isolate it works for as a V8 helper.
pub fn check_isolates(
    context: &str,
    reported: &[Thread],
    snapshot: &StateSnapshot,
) -> BTreeMap<u64, u64> {
    let mut isolates = BTreeMap::new();
    let mut helpers = Vec::new();
    for thread in snapshot.threads.iter() {
        let id = thread.id.get();
        let labels: Vec<String> = thread.labels.iter().map(ToString::to_string).collect();
        let Some(javascript) = reported.iter().find(|r| r.native == id) else {
            helpers.push((id, labels));
            continue;
        };
        let [label] = &thread.labels[..] else {
            panic!(
                "{context}: {} has one label: {:?}",
                javascript.name, thread.labels
            );
        };
        let number = match &label.value {
            Ok(number) if &*label.kind == "isolate" => number.parse().expect("a number"),
            _ => panic!("{context}: {} has the label {label}", javascript.name),
        };
        isolates.insert(id, number);
    }

    let numbers: std::collections::BTreeSet<u64> = isolates.values().copied().collect();
    assert_eq!(numbers.len(), reported.len(), "{context}: {isolates:?}");
    let shown: Vec<String> = numbers.iter().map(|n| format!("isolate {n}")).collect();
    for (id, labels) in helpers {
        assert!(
            labels.len() <= 1 && labels.iter().all(|label| shown.contains(label)),
            "{context}: thread {id} runs no JavaScript, and works for no isolate a \
             JavaScript thread runs: {labels:?}"
        );
    }
    assert_eq!(
        isolates.get(&reported[0].native),
        Some(&1),
        "{context}: main"
    );
    isolates
}

/// On every build, `stacks.js` parks its main thread and two workers in
/// the functions it names, each thread one the stop lists and labelled with
/// its isolate, the same at a second stop, and the process ends with the
/// debugger.
#[tokio::test]
async fn each_builds_threads_park_where_the_fixture_says() {
    for entry in MATRIX {
        let mut session = NodeSession::launch(entry, "stacks.js", &[], &[]).await;
        let context = session.context();
        let reported = threads(&session.stop_at("parked").await);
        check_stacks(&context, &reported);

        let snapshot = session.stopped().await;
        let InferiorState::Stopped { process_id, .. } = snapshot.inferior else {
            panic!("{context}: not stopped");
        };
        assert_eq!(reported[0].native, process_id.get(), "{context}");
        let listed: Vec<u64> = snapshot.threads.iter().map(|t| t.id.get()).collect();
        check_listed(&context, &reported, &listed);
        let isolates = check_isolates(&context, &reported, &snapshot);

        session.resume().await;
        session.pause().await;
        let again = session.stopped().await;
        assert_eq!(
            check_isolates(&context, &reported, &again),
            isolates,
            "{context}: the second stop's isolates"
        );

        let report = session.report();
        let process = report
            .lines()
            .find(|fields| fields[0] == "process")
            .expect("the process line");
        assert_eq!(process[1], process_id.get().to_string(), "{context}");
        assert!(
            process[2].starts_with(&format!("{}.", major(entry))),
            "{context}: {process:?}"
        );

        session.shutdown().await;
    }
}

/// On every build, `tiers.js` puts a frame of each tier the build can run
/// on one stack, and says which it cannot.
#[tokio::test]
async fn each_build_reports_each_tier_from_inside_its_frame() {
    for entry in MATRIX {
        let mut session =
            NodeSession::launch(entry, "tiers.js", &["--allow-natives-syntax"], &[]).await;
        let context = session.context();
        let lines = session.stop_at("tiers").await;
        check_tiers(&context, entry, &session.report(), &lines);
        session.shutdown().await;
    }
}

/// Each core dump holds every thread its fixture reported just before it
/// aborted, each labelled with its isolate, and the report beside it says
/// what the live one does.
#[tokio::test]
async fn a_cores_threads_are_the_ones_node_reported() {
    for entry in CORES {
        for (fixture, checkpoint) in [("stacks", "parked"), ("tiers", "tiers")] {
            let name = format!("node-{fixture}-{entry}.core");
            let core = Scenario::fixture(&name);
            let log = std::fs::read_to_string(core.with_extension("core.log"))
                .expect("the core's log; run `just build-test-programs`");
            let report = Report::parse(&log);
            let lines = report
                .checkpoint(checkpoint)
                .expect("the checkpoint the core was taken at")
                .to_vec();
            let reported = threads(&lines);
            if fixture == "stacks" {
                check_stacks(&name, &reported);
            } else {
                check_tiers(&name, entry, &report, &lines);
            }

            let mut scenario = Scenario::open_core(&name, &CoreDumpOptions::new(core));
            let snapshot = scenario.snapshot().await;
            let InferiorState::Stopped { process_id, .. } = snapshot.inferior else {
                panic!("{name}: the core is not stopped");
            };
            assert_eq!(reported[0].native, process_id.get(), "{name}");
            let listed: Vec<u64> = snapshot.threads.iter().map(|t| t.id.get()).collect();
            check_listed(&name, &reported, &listed);
            check_isolates(&name, &reported, &snapshot);
            assert!(
                listed.len() > reported.len(),
                "{name}: Node's own threads: {listed:?}"
            );

            scenario.shutdown().await;
        }
    }
}
