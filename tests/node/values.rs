//! JavaScript values as microscope presents them, held to the text
//! `util.inspect` gave the same values at the same checkpoint.

use microscope::{Evaluation, Expression, SourceLanguage, ThreadId, VariableState};

use crate::session::{MATRIX, NodeSession};
use crate::truth::threads;

/// Selects the frame of `function` on the printer's thread.
async fn select_function(session: &NodeSession, native: u64, function: &str) {
    let handle = session.scenario.handle();
    session
        .scenario
        .operation("select", handle.select_context(ThreadId::new(native)))
        .await;
    let trace = session
        .scenario
        .operation("backtrace", handle.backtrace())
        .await;
    let frame = trace
        .frames
        .iter()
        .find(|frame| {
            frame.recorded.as_ref().is_some_and(|recorded| {
                recorded.language == SourceLanguage::JavaScript
                    && recorded.function.as_ref() == function
            })
        })
        .unwrap_or_else(|| panic!("{}: no frame of {function}", session.context()));
    session
        .scenario
        .operation("select frame", handle.select_frame(frame.id))
        .await;
}

/// The summary microscope presents `text` as, or what kept it from one.
async fn summary(session: &NodeSession, text: &str) -> String {
    let expression = Expression::parse(text).unwrap_or_else(|error| panic!("{text}: {error}"));
    match session.scenario.handle().evaluate(&expression).await {
        Ok(Evaluation::Value { value, .. }) => match value.state {
            VariableState::Available {
                presentation: Some(presentation),
                ..
            } => presentation.summary.to_string(),
            state => format!("<{state:?}>"),
        },
        Ok(other) => format!("<{other:?}>"),
        Err(error) => format!("<error: {error}>"),
    }
}

/// At most the first 300 characters of `text`, for a message.
fn clipped(text: &str) -> String {
    match text.char_indices().nth(300) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

/// On every build, each value of the gallery is presented as the text
/// `util.inspect` gave it.
#[tokio::test]
async fn gallery_values_are_the_text_node_gave_them() {
    let mut lies = Vec::new();
    for entry in MATRIX {
        let mut session = NodeSession::launch(entry, "values.js", &[], &[]).await;
        let reported = threads(&session.stop_at("gallery").await);
        let printer = reported
            .iter()
            .find(|thread| thread.printer)
            .expect("the printer's thread");
        select_function(&session, printer.native, "gallery").await;
        for (name, canonical) in &printer.values {
            let shown = summary(&session, name).await;
            if &shown != canonical {
                lies.push(format!(
                    "{}: `{name}`\n  node:  {:?}\n  shown: {:?}",
                    session.context(),
                    clipped(canonical),
                    clipped(&shown),
                ));
            }
        }
        session.shutdown().await;
    }
    assert!(lies.is_empty(), "{} lies:\n{}", lies.len(), lies.join("\n"));
}
