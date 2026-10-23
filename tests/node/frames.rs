//! JavaScript frames as microscope names them, held to what V8's own call
//! sites reported at the same checkpoint.

use microscope::{Backtrace, SourceLanguage, ThreadId};

use microscope::CoreDumpOptions;

use crate::session::{CORES, MATRIX, NodeSession};
use crate::support::Scenario;
use crate::support::truth::Report;
use crate::truth::{Frame, Thread, check_threads, threads};

/// The JavaScript frames of a backtrace, innermost first, past those
/// `truth.js` and Node's `fs`, which the fixture parks in, run.
pub fn javascript(trace: &Backtrace) -> Vec<Frame> {
    javascript_recorded(trace)
        .into_iter()
        .map(|(frame, _)| frame)
        .collect()
}

/// The JavaScript frames of a backtrace as [`javascript`] gives them, each
/// with the problem that kept its place unknown, if one did. A frame with a
/// problem is never taken for V8's own.
pub fn javascript_recorded(trace: &Backtrace) -> Vec<(Frame, Option<String>)> {
    trace
        .frames
        .iter()
        .filter_map(|frame| frame.recorded.as_ref())
        .filter(|recorded| {
            matches!(
                recorded.language,
                SourceLanguage::JavaScript | SourceLanguage::TypeScript
            )
        })
        .map(|recorded| {
            let place = recorded.place.as_ref();
            let frame = Frame {
                function: if recorded.function.is_empty() {
                    "-".to_owned()
                } else {
                    recorded.function.to_string()
                },
                file: place.map_or_else(|| "-".to_owned(), |place| place.path.to_string()),
                line: place.map(|place| place.line),
                column: place.map(|place| place.column),
                tier: None,
            };
            (frame, recorded.problem.as_deref().map(str::to_owned))
        })
        .skip_while(|(frame, problem)| {
            frame.file.ends_with("/tests/fixtures/node/truth.js")
                || frame.file.starts_with("node:fs")
                || (frame.file == "-" && problem.is_none())
        })
        .collect()
}

/// Each reported thread as microscope reads it at the stop.
pub async fn observed(scenario: &Scenario, reported: &[Thread]) -> Vec<Thread> {
    let mut observed = Vec::new();
    for thread in reported {
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
        observed.push(Thread {
            frames: javascript(&trace),
            values: Vec::new(),
            ..thread.clone()
        });
    }
    observed
}

/// On every build, each parked thread's interpreted frames are the
/// functions, files, lines, and columns V8 reported.
#[tokio::test]
async fn each_threads_javascript_frames_are_the_ones_v8_reported() {
    let mut lies = Vec::new();
    for entry in MATRIX {
        let mut session = NodeSession::launch(entry, "stacks.js", &[], &[]).await;
        let reported = threads(&session.stop_at("parked").await);
        let seen = observed(&session.scenario, &reported).await;
        if let Err(lie) = check_threads(&seen, &reported) {
            lies.push(format!("{}: {lie}", session.context()));
        }
        session.shutdown().await;
    }
    assert_truthful(&lies);
}

/// On every build, the frames of an ES module and of the TypeScript module
/// it imports, which V8 names by percent-encoded `file:` URLs, are at the
/// paths, lines, and columns V8 reported, and each is in its own language.
#[tokio::test]
async fn module_frames_are_at_the_paths_their_urls_name() {
    let mut lies = Vec::new();
    for entry in MATRIX {
        let mut session = NodeSession::launch(entry, "modules é/entry.mjs", &[], &[]).await;
        let reported = threads(&session.stop_at("modules").await);
        let seen = observed(&session.scenario, &reported).await;
        if let Err(lie) = check_threads(&seen, &reported) {
            lies.push(format!("{}: {lie}", session.context()));
        }
        let trace = session
            .scenario
            .operation("backtrace", session.scenario.handle().backtrace())
            .await;
        let languages = trace
            .frames
            .iter()
            .filter_map(|frame| frame.recorded.as_ref())
            .filter_map(|recorded| {
                let path = &recorded.place.as_ref()?.path;
                let file = path.rsplit('/').next()?;
                path.contains("/modules é/")
                    .then(|| (file.to_owned(), recorded.language))
            })
            .collect::<Vec<_>>();
        let expected = [
            ("entry.mjs", SourceLanguage::JavaScript),
            ("helper.mts", SourceLanguage::TypeScript),
            ("entry.mjs", SourceLanguage::JavaScript),
            ("entry.mjs", SourceLanguage::JavaScript),
        ]
        .map(|(file, language)| (file.to_owned(), language));
        if languages != expected {
            lies.push(format!("{}: languages {languages:?}", session.context()));
        }
        session.shutdown().await;
    }
    assert_truthful(&lies);
}

/// On every build, the frames of `tiers.js` in each tier the build runs
/// (interpreted, Sparkplug, Maglev, Turbofan, inlined, deoptimized, and
/// entered on a loop) are the ones V8 reported, the inlined ones included.
#[tokio::test]
async fn frames_of_every_tier_are_the_ones_v8_reported() {
    let mut lies = Vec::new();
    for entry in MATRIX {
        let mut session =
            NodeSession::launch(entry, "tiers.js", &["--allow-natives-syntax"], &[]).await;
        let mut reported = threads(&session.stop_at("tiers").await);
        // The tier is what makes each frame's reading differ, not a fact
        // a backtrace reports.
        for frame in reported.iter_mut().flat_map(|thread| &mut thread.frames) {
            frame.tier = None;
        }
        let seen = observed(&session.scenario, &reported).await;
        if let Err(lie) = check_threads(&seen, &reported) {
            lies.push(format!("{}: {lie}", session.context()));
        }
        session.shutdown().await;
    }
    assert_truthful(&lies);
}

/// In each core dump, every reported thread's JavaScript frames are the
/// ones V8 reported just before the process aborted: the post-mortem
/// reader reads Node's own scripts' source from the executable.
#[tokio::test]
async fn a_cores_javascript_frames_are_the_ones_v8_reported() {
    let mut lies = Vec::new();
    for entry in CORES {
        for (fixture, checkpoint) in [("stacks", "parked"), ("tiers", "tiers")] {
            let name = format!("node-{fixture}-{entry}.core");
            let core = Scenario::fixture(&name);
            let log = std::fs::read_to_string(core.with_extension("core.log"))
                .expect("the core's log; run `just build-test-programs`");
            let mut reported = threads(
                Report::parse(&log)
                    .checkpoint(checkpoint)
                    .expect("the checkpoint the core was taken at"),
            );
            for frame in reported.iter_mut().flat_map(|thread| &mut thread.frames) {
                frame.tier = None;
            }
            let scenario = Scenario::open_core(&name, &CoreDumpOptions::new(core));
            let seen = observed(&scenario, &reported).await;
            if let Err(lie) = check_threads(&seen, &reported) {
                lies.push(format!("{name}: {lie}"));
            }
            scenario.shutdown().await;
        }
    }
    assert_truthful(&lies);
}

/// What N1 does not read yet, each as text a lie about it contains and
/// why. Every other lie fails a test; these are printed, and leave with the
/// work that ends them.
const GAPS: &[(&[&str], &str)] = &[];

/// Fails on any lie but the known gaps', which it prints.
fn assert_truthful(lies: &[String]) {
    let (known, unknown): (Vec<_>, Vec<_>) = lies.iter().partition(|lie| {
        GAPS.iter()
            .any(|(texts, _)| texts.iter().all(|text| lie.contains(text)))
    });
    for lie in known {
        let why = GAPS
            .iter()
            .find(|(texts, _)| texts.iter().all(|text| lie.contains(text)))
            .map_or("", |(_, why)| *why);
        eprintln!("known gap ({why}): {lie}");
    }
    assert!(unknown.is_empty(), "{unknown:#?}");
}
