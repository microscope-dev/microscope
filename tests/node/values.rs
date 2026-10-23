//! Checks the JavaScript values microscope presents against the text
//! `util.inspect` gave them at the same checkpoint.

use microscope::{Evaluation, Expression, SourceLanguage, ThreadId, VariableKind, VariableState};

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

/// The summary and full text microscope presents for `text`, or why it
/// could not.
pub async fn presented(session: &NodeSession, text: &str) -> (String, String) {
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

/// The summary expected for `value`: an error's first line, else its
/// one-line text cut at any error stack inside it.
fn summary_of(value: &Named) -> String {
    match value.line.split_once('\n') {
        None => value.line.clone(),
        Some(_) if value.error => value.text.lines().next().unwrap_or_default().to_owned(),
        Some((first, _)) => format!("{first} …"),
    }
}

/// Each way the shown summary and text differ from what Node gave `value`.
pub fn lies_about(context: &str, value: &Named, (summary, text): &(String, String)) -> Vec<String> {
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

/// On every build, each gallery value shows the text `util.inspect` gave
/// it. Errors whose stacks were never read show the stacks Node formats
/// from their call sites once something reads them.
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
        // At "gallery", `unread` holds raw call sites. Node formats them
        // when the fixture inspects `unread` at the next checkpoint.
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

/// The kind of each variable of the selected frame named in `names`, in
/// the order asked, `None` for one the frame does not list.
async fn kinds(session: &NodeSession, names: &[&str]) -> Vec<Option<VariableKind>> {
    let snapshot = session
        .scenario
        .operation("variables", session.scenario.handle().variables())
        .await;
    names
        .iter()
        .map(|name| {
            snapshot
                .variables
                .iter()
                .find(|variable| &*variable.name == *name)
                .map(|variable| variable.kind)
        })
        .collect()
}

/// On every build, a JavaScript frame's variables are in JavaScript's
/// scopes: receiver and arguments, its own contexts, what its closure
/// captured, a module's bindings, `globalThis`, and interpreter registers,
/// which say why they have no names.
#[tokio::test]
async fn frame_variables_are_in_the_scopes_javascript_gives_them() {
    use VariableKind::{Captured, Global, Local, Module, Parameter, Unnamed};
    let mut lies = Vec::new();
    for entry in MATRIX {
        let mut session = NodeSession::launch(entry, "values.js", &[], &[]).await;
        let reported = threads(&session.stop_at("gallery").await);
        let printer = reported.iter().find(|t| t.printer).expect("the printer");
        select_function(&session, printer.native, "gallery").await;
        // `values` is gallery's own, which its arrow captures; `truth` is
        // the CommonJS wrapper's, around it.
        let names = [
            "this",
            "arguments[0]",
            "values",
            "truth",
            "globalThis",
            "r0",
        ];
        let expected = [Parameter, Parameter, Local, Captured, Global, Unnamed].map(Some);
        let seen = kinds(&session, &names).await;
        if seen != expected {
            lies.push(format!(
                "{}: gallery's {names:?} are {seen:?}",
                session.context()
            ));
        }
        let snapshot = session
            .scenario
            .operation("variables", session.scenario.handle().variables())
            .await;
        let unnamed = snapshot
            .variables
            .iter()
            .filter(|variable| variable.kind == Unnamed)
            .all(|variable| variable.unnamed.is_some());
        if !unnamed {
            lies.push(format!(
                "{}: a register does not say why it has no name",
                session.context()
            ));
        }
        session.proceed().await;
        session.stop_at("unread").await;
        session.shutdown().await;

        let mut session = NodeSession::launch(entry, "modules é/entry.mjs", &[], &[]).await;
        let reported = threads(&session.stop_at("modules").await);
        let printer = reported.iter().find(|t| t.printer).expect("the printer");
        select_function(&session, printer.native, "settle").await;
        let names = ["entries", "label"];
        let seen = kinds(&session, &names).await;
        if seen != [Some(Local), Some(Module)] {
            lies.push(format!(
                "{}: settle's {names:?} are {seen:?}",
                session.context()
            ));
        }
        session.shutdown().await;
    }
    assert!(lies.is_empty(), "{} lies:\n{}", lies.len(), lies.join("\n"));
}

/// On every build, `.length`, `.size`, and typed array elements read what
/// JavaScript would give for the gallery's values, as the fixture writes
/// them, and an element past a typed array's end is absent.
#[tokio::test]
async fn lengths_sizes_and_typed_elements_are_javascripts() {
    let cases = [
        ("values.arrays.packedSmi.length", "3"),
        ("values.arrays.long.length", "150"),
        ("values.arrays.dictionary.length", "100001"),
        ("values.strings.oneByte.length", "5"),
        // 'h\u00e9llo \u2603 \u{1f600}': the emoji is two UTF-16 units.
        ("values.strings.twoByte.length", "10"),
        ("values.collections.map.size", "2"),
        ("values.collections.set.size", "3"),
        ("values.collections.emptyMap.size", "0"),
        ("values.binary.uint8.length", "3"),
        ("values.binary.uint8[2]", "255"),
        ("values.binary.int32[0]", "-1"),
        ("values.binary.int32[2]", "2147483647"),
        ("values.binary.float64[0]", "0.1"),
        ("values.binary.view.length", "3"),
        ("values.binary.buffer.length", "12"),
        // 'buffer bytes' starts with b, 98.
        ("values.binary.buffer[0]", "98"),
    ];
    let mut lies = Vec::new();
    for entry in MATRIX {
        let mut session = NodeSession::launch(entry, "values.js", &[], &[]).await;
        let reported = threads(&session.stop_at("gallery").await);
        let printer = reported
            .iter()
            .find(|thread| thread.printer)
            .expect("the printer's thread");
        select_function(&session, printer.native, "gallery").await;
        for (text, expected) in cases {
            let (summary, _) = presented(&session, text).await;
            if summary != expected {
                lies.push(format!(
                    "{}: {text} is {summary}, not {expected}",
                    session.context()
                ));
            }
        }
        let (past, _) = presented(&session, "values.binary.uint8[3]").await;
        if !past.starts_with("<error") {
            lies.push(format!("{}: uint8[3] is {past}", session.context()));
        }
        session.shutdown().await;
    }
    assert!(lies.is_empty(), "{} lies:\n{}", lies.len(), lies.join("\n"));
}

/// On every build, `process.env`, whose variables an interceptor gives, is
/// refused with that reason rather than shown as `{}`, and so is a member
/// of it rather than reported absent.
#[tokio::test]
async fn an_interceptors_object_is_refused_rather_than_shown_empty() {
    let mut wrong = Vec::new();
    for entry in MATRIX {
        let mut session = NodeSession::launch(entry, "interceptors.js", &[], &[]).await;
        let reported = threads(&session.stop_at("look").await);
        let printer = reported
            .iter()
            .find(|thread| thread.printer)
            .expect("the printer's thread");
        select_function(&session, printer.native, "look").await;
        for text in ["env", "env.HOME"] {
            let (summary, _) = presented(&session, text).await;
            if !summary.contains("an interceptor gives this object's properties") {
                wrong.push(format!("{}: {text} is {summary}", session.context()));
            }
        }
        session.shutdown().await;
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
