//! JavaScript values as microscope presents them, held to the text
//! `util.inspect` gave the same values at the same checkpoint.

use microscope::{Evaluation, Expression, SourceLanguage, ThreadId, VariableState};

use crate::session::{MATRIX, NodeSession};
use crate::truth::{Named, threads};

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

/// The summary and the full text microscope presents `text` with, or what
/// kept it from them.
async fn presented(session: &NodeSession, text: &str) -> (String, String) {
    let expression = Expression::parse(text).unwrap_or_else(|error| panic!("{text}: {error}"));
    match session.scenario.handle().evaluate(&expression).await {
        Ok(Evaluation::Value { value, .. }) => match value.state {
            VariableState::Available {
                presentation: Some(presentation),
                ..
            } => (
                presentation.summary.to_string(),
                presentation.text().to_string(),
            ),
            state => twice(format!("<{state:?}>")),
        },
        Ok(other) => twice(format!("<{other:?}>")),
        Err(error) => twice(format!("<error: {error}>")),
    }
}

fn twice(text: String) -> (String, String) {
    (text.clone(), text)
}

/// The one line a summary shows of `value`: an error's first line, else
/// its text with every entry on one line, cut at an error stack it holds.
fn summary_of(value: &Named) -> String {
    match value.line.split_once('\n') {
        None => value.line.clone(),
        Some(_) if value.error => value.text.lines().next().unwrap_or_default().to_owned(),
        Some((first, _)) => format!("{first} …"),
    }
}

/// The lies in presenting `value` as `shown`, the summary and the text.
fn lies_about(context: &str, value: &Named, (summary, text): &(String, String)) -> Vec<String> {
    let mut lies = Vec::new();
    let name = &value.name;
    if text != &value.text {
        lies.push(format!(
            "{context}: `{name}`\n  node:  {:?}\n  shown: {:?}",
            clipped(&value.text),
            clipped(text),
        ));
    }
    let line = summary_of(value);
    if summary != &line {
        lies.push(format!(
            "{context}: `{name}`'s summary\n  node:  {:?}\n  shown: {:?}",
            clipped(&line),
            clipped(summary),
        ));
    }
    lies
}

/// At most the first 300 characters of `text`, for a message.
fn clipped(text: &str) -> String {
    match text.char_indices().nth(300) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

/// On every build, each value of the gallery is presented as the text
/// `util.inspect` gave it, and the errors whose stacks nothing read are
/// presented with the stacks Node formats from their call sites once
/// something does.
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
        for value in &printer.values {
            let shown = presented(&session, &value.name).await;
            lies.extend(lies_about(&session.context(), value, &shown));
        }
        // What the program holds in `unread` at "gallery" is raw call
        // sites, which no one has formatted; Node formats them when the
        // fixture inspects `unread` at the next checkpoint.
        let unformatted = presented(&session, "unread").await;
        session.proceed().await;
        let reported = threads(&session.stop_at("unread").await);
        let printer = reported
            .iter()
            .find(|thread| thread.printer)
            .expect("the printer's thread");
        for value in printer.values.iter().filter(|value| value.name == "unread") {
            let context = format!("{}, formatted from its call sites", session.context());
            lies.extend(lies_about(&context, value, &unformatted));
        }
        session.shutdown().await;
    }
    assert!(lies.is_empty(), "{} lies:\n{}", lies.len(), lies.join("\n"));
}
