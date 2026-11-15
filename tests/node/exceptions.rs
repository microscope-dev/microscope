use std::collections::BTreeMap;

use microscope::{
    ExceptionStops, ExitStatus, LanguageException, LanguageExceptionKind, StopReason,
};

use crate::oracle::{self, Pause, Skipping};
use crate::session::{LINES, NodeSession};

const FIXTURE: &str = "exceptions.js";

const CHOICES: [(&[&str], &str); 3] = [
    (&["javascript-thrown"], "all"),
    (&["javascript-uncaught"], "uncaught"),
    (&["javascript-thrown", "javascript-uncaught"], "all"),
];

async fn run(entry: &str, chosen: &[&str]) -> (Vec<Pause>, Vec<String>) {
    let session = NodeSession::unlaunched(entry, FIXTURE);
    let mut stops = ExceptionStops::NONE;
    for filter in chosen {
        stops = stops.with(filter, true).expect("a filter");
    }
    session
        .scenario
        .operation(
            "choose exceptions",
            session.scenario.handle().set_exception_stops(stops),
        )
        .await;
    let (mut session, mut reason, _events) = session.run_to_stop(Vec::new()).await;
    let context = session.context();
    let (mut seen, mut messages) = (Vec::new(), Vec::new());
    loop {
        match &reason {
            StopReason::LanguageException(LanguageException { kind, message, .. }) => {
                let mut pause = oracle::selected(&session.scenario).await;
                pause.uncaught = Some(match kind {
                    LanguageExceptionKind::Unhandled => true,
                    LanguageExceptionKind::Raised => false,
                    LanguageExceptionKind::Fatal => panic!("{context}: a fatal stop"),
                });
                seen.push(pause);
                messages.push(message.to_string());
            }
            StopReason::Exited(ExitStatus::Code(1)) => break,
            other => panic!("{context}: {other:?}\n{}", session.output()),
        }
        assert!(seen.len() < 16, "{context}: too many stops");
        reason = session.scenario.resume_to_stop().await;
    }
    let output = session.output();
    for line in ["parsed SyntaxError exists false", "RangeError: too big: 3"] {
        assert!(output.contains(line), "{context}: no {line:?} in {output}");
    }
    session.shutdown().await;
    (seen, messages)
}

fn reported(expected: Vec<Pause>, chosen: &[&str]) -> Vec<Pause> {
    expected
        .into_iter()
        .map(|mut pause| {
            if !chosen.contains(&"javascript-uncaught") {
                pause.uncaught = pause.uncaught.map(|_| false);
            }
            pause
        })
        .collect()
}

pub async fn lies(entry: &str) -> Vec<String> {
    let mut lies = Vec::new();
    let mut expected = BTreeMap::new();
    for (chosen, state) in CHOICES {
        let expected = expected.entry(state).or_insert_with(|| {
            oracle::pauses(entry, FIXTURE, &[], (&[], Skipping::default()), state)
        });
        let (seen, messages) = run(entry, chosen).await;
        let expected = reported(expected.clone(), chosen);
        let differences = oracle::differences(&seen, &expected);
        if !differences.is_empty() {
            lies.push(format!(
                "node {entry} {chosen:?}:\n  {}",
                differences.join("\n  ")
            ));
        }
        let last = messages.last().map_or("", String::as_str);
        if !last.starts_with("RangeError: too big: 3\n    at check (") {
            lies.push(format!(
                "node {entry} {chosen:?}: the last message is {last:?}"
            ));
        }
    }
    lies
}

#[tokio::test]
async fn javascript_exceptions_stop_where_v8_pauses_at_them() {
    let mut found = Vec::new();
    for entry in LINES {
        found.extend(lies(entry).await);
    }
    assert!(found.is_empty(), "{}", found.join("\n"));
}
