use microscope::{Backtrace, SourceLanguage, StackFrame, StackSegment, ThreadId, VariableKind};

use crate::native::check_thread;
use crate::session::{MATRIX, NodeSession};
use crate::support::Scenario;
use crate::truth::{Frame, Thread, check_threads, threads};

pub fn javascript(trace: &Backtrace) -> Vec<Frame> {
    javascript_recorded(trace)
        .into_iter()
        .map(|(frame, _)| frame)
        .collect()
}

pub fn javascript_recorded(trace: &Backtrace) -> Vec<(Frame, Option<String>)> {
    recorded(
        trace
            .frames
            .iter()
            .filter(|frame| frame.segment != StackSegment::Awaiting),
    )
    .skip_while(|(frame, problem)| {
        frame.file.ends_with("/tests/fixtures/node/truth.js")
            || frame.file.starts_with("node:fs")
            || (frame.file == "-" && problem.is_none())
    })
    .collect()
}

pub fn awaiting_recorded(trace: &Backtrace) -> Vec<(Frame, Option<String>)> {
    recorded(
        trace
            .frames
            .iter()
            .filter(|frame| frame.segment == StackSegment::Awaiting),
    )
    .collect()
}

pub fn awaiting(trace: &Backtrace) -> Vec<Frame> {
    awaiting_recorded(trace)
        .into_iter()
        .map(|(frame, _)| frame)
        .collect()
}

fn recorded<'a>(
    frames: impl Iterator<Item = &'a StackFrame>,
) -> impl Iterator<Item = (Frame, Option<String>)> {
    frames
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
}

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
            awaiting: awaiting(&trace),
            values: Vec::new(),
            ..thread.clone()
        });
    }
    observed
}

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

        let printer = reported.iter().find(|t| t.printer).expect("the printer");
        crate::values::select_function(&session, printer.native, "settle").await;
        let names = ["entries", "label"];
        let kinds = crate::values::kinds(&session, &names).await;
        if kinds != [Some(VariableKind::Local), Some(VariableKind::Module)] {
            lies.push(format!(
                "{}: settle's {names:?} are {kinds:?}",
                session.context()
            ));
        }
        session.shutdown().await;
    }
    assert_truthful(&lies);
}

#[tokio::test]
async fn frames_of_every_tier_are_the_ones_v8_reported() {
    let mut lies = Vec::new();
    for entry in MATRIX {
        let mut session =
            NodeSession::launch(entry, "tiers.js", &["--allow-natives-syntax"], &[]).await;
        let checkpoint = session.stop_at("tiers").await;
        crate::stacks::check_tiers(&session.context(), entry, &session.report(), &checkpoint);
        let mut reported = threads(&checkpoint);
        let printer_native = reported
            .iter()
            .find(|t| t.printer)
            .expect("the printer")
            .native;
        crate::values::optimized_frames_show_no_stale_registers(&session, printer_native).await;
        for frame in reported.iter_mut().flat_map(|thread| &mut thread.frames) {
            frame.tier = None;
        }
        let seen = observed(&session.scenario, &reported).await;
        if let Err(lie) = check_threads(&seen, &reported) {
            lies.push(format!("{}: {lie}", session.context()));
        }

        let scenario = &session.scenario;
        let modules = scenario
            .operation("modules", scenario.handle().loaded_modules())
            .await;
        scenario
            .operation(
                "select",
                scenario
                    .handle()
                    .select_context(ThreadId::new(printer_native)),
            )
            .await;
        let trace = scenario
            .operation("backtrace", scenario.handle().backtrace())
            .await;
        let context = session.context();
        let (compiled, _) = check_thread(&context, true, &trace, &modules);
        assert!(compiled >= 2, "{context}: {compiled} compiled frames");
        session.shutdown().await;
    }
    assert_truthful(&lies);
}

const GAPS: &[(&[&str], &str)] = &[];

pub fn assert_truthful(lies: &[String]) {
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
