//! JavaScript exceptions through Node's inspector, held to where V8 pauses
//! at the same exceptions under a plain CDP client (`oracle.rs`), for each
//! choice of filters: where each stops, whether it is the caught or the
//! uncaught kind, and that an exception Node's own code catches never
//! stops.

use microscope::{
    ExceptionStops, ExitStatus, LanguageException, LanguageExceptionKind, StopReason,
};

use crate::oracle::{self, Pause, Skipping};
use crate::session::{MATRIX, NodeSession};

const FIXTURE: &str = "exceptions.js";

/// Each choice of filters, and the state V8 is given for it.
const CHOICES: [(&[&str], &str); 3] = [
    (&["javascript-thrown"], "all"),
    (&["javascript-uncaught"], "uncaught"),
    (&["javascript-thrown", "javascript-uncaught"], "all"),
];

/// Runs the fixture on one build with the filters `chosen` and returns
/// where microscope stopped and where V8 paused, and the messages.
async fn run(entry: &str, chosen: &[&str], state: &str) -> (Vec<Pause>, Vec<Pause>, Vec<String>) {
    let expected = oracle::pauses(entry, FIXTURE, &[], (&[], Skipping::default()), state);
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
    (seen, expected, messages)
}

/// The uncaught filter reports the uncaught kind when both are chosen.
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

/// On every build and for every choice, each exception stops where V8
/// pauses at it, as the kind it is, with V8's message.
#[tokio::test]
async fn javascript_exceptions_stop_where_v8_pauses_at_them() {
    let mut lies = Vec::new();
    for entry in MATRIX {
        for (chosen, state) in CHOICES {
            let (seen, expected, messages) = run(entry, chosen, state).await;
            let expected = reported(expected, chosen);
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
    }
    assert!(lies.is_empty(), "{}", lies.join("\n"));
}
