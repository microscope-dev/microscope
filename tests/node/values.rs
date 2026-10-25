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

/// What the user sees for `text`: a computed value's one-line summary and
/// its type's name, or the error.
async fn shown(session: &NodeSession, text: &str) -> String {
    let expression = Expression::parse(text).unwrap_or_else(|error| panic!("{text}: {error}"));
    match session.scenario.handle().evaluate(&expression).await {
        Ok(Evaluation::Value { value, .. }) => {
            let summary = microscope::value_summary(value.type_info.as_ref(), &value.state);
            let ty = value
                .type_info
                .map_or_else(String::new, |info| info.name.to_string());
            format!("{summary} : {ty}")
        }
        Ok(other) => format!("<{other:?}>"),
        Err(microscope::Error::Expression(error)) => {
            format!("<{} error: {}>", error.kind.name(), error.message)
        }
        Err(error) => format!("<error: {error}>"),
    }
}

/// On every build, operators on JavaScript values give what JavaScript
/// gives: every Number is a double, a `BigInt` mixes with no Number, and
/// what microscope cannot compute is refused.
#[tokio::test]
async fn operators_compute_as_javascript_does() {
    let cases = [
        ("values.numbers.smi / 5", "8.4 : number"),
        ("values.numbers.smi % -5", "2 : number"),
        ("values.numbers.negative % 3", "-1 : number"),
        // 2^53 + 3 rounds to even, 2^53 + 4.
        ("values.numbers.unsafe + 1", "9007199254740996 : number"),
        (
            "values.numbers.smi * values.numbers.smi * 1e20",
            "1.764e+23 : number",
        ),
        ("values.numbers.smi / 0", "Infinity : number"),
        ("-values.numbers.smi / 0", "-Infinity : number"),
        ("values.numbers.negativeZero * 1", "-0 : number"),
        ("values.numbers.large * 1", "1e+21 : number"),
        ("values.numbers.smi | 0x80000000", "-2147483606 : number"),
        ("1 << values.numbers.smi", "1024 : number"),
        (
            "values.bigints.negative * 2n",
            "-24691357802469135780n : bigint",
        ),
        ("-values.bigints.small", "-1n : bigint"),
        (
            "values.bigints.negative / 7n",
            "-1763668414462081127n : bigint",
        ),
        (
            "values.bigints.small + 1",
            "<type error: TypeError: Cannot mix BigInt and other types, use explicit conversions>",
        ),
        ("values.bigints.small == 1", "true : bool"),
        (
            "values.primitives.null == values.primitives.undefined",
            "true : bool",
        ),
        ("values.primitives.undefined + 1", "NaN : number"),
        ("!values.collections.emptyMap", "false : bool"),
        (
            "values.arrays.packedSmi[values.numbers.smi - 40]",
            "3 : number",
        ),
        (
            "values.strings.oneByte + 1",
            "<unsupported error: `+` with a string joins text into a new string, which microscope \
             does not make>",
        ),
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
            let actual = shown(&session, text).await;
            if actual != expected {
                lies.push(format!(
                    "{}: {text} is {actual}, not {expected}",
                    session.context()
                ));
            }
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

/// On every build, a frame running optimized code shows no interpreter
/// registers, whose slots it leaves stale, not even one entered on a loop
/// (OSR) that kept the interpreter's frame; a baseline or interpreted frame
/// shows its registers.
#[tokio::test]
async fn optimized_frames_show_no_stale_registers() {
    let mut lies = Vec::new();
    for entry in MATRIX {
        let mut session =
            NodeSession::launch(entry, "tiers.js", &["--allow-natives-syntax"], &[]).await;
        let reported = threads(&session.stop_at("tiers").await);
        let printer = reported.iter().find(|t| t.printer).expect("the printer");
        // `deoptimized` left its optimized code for the interpreter.
        for (function, registers) in [
            ("osr", false),
            ("turbofan", false),
            ("sparkplug", true),
            ("deoptimized", true),
        ] {
            select_function(&session, printer.native, function).await;
            let snapshot = session
                .scenario
                .operation("variables", session.scenario.handle().variables())
                .await;
            let shown = snapshot
                .variables
                .iter()
                .any(|variable| variable.kind == VariableKind::Unnamed);
            if shown != registers {
                lies.push(format!(
                    "{}: {function} shows registers: {shown}",
                    session.context()
                ));
            }
        }
        session.shutdown().await;
    }
    assert!(lies.is_empty(), "{lies:#?}");
}

#[tokio::test]
async fn inspection_keys_follow_ordinary_and_integer_indexed_rules() {
    let mut lies = Vec::new();
    for entry in MATRIX {
        let mut session = NodeSession::launch(entry, "values.js", &[], &["inspection"]).await;
        let reported = threads(&session.stop_at("inspection").await);
        let printer = reported.iter().find(|t| t.printer).expect("the printer");
        select_function(&session, printer.native, "inspection").await;
        for (expression, expected) in [
            ("keyed[4294967295]", "42"),
            ("keyed[\"4294967295\"]", "42"),
            ("array[4294967295]", "43"),
            ("typed[0]", "2"),
            ("typed[\"01\"]", "98"),
            ("typed[\"-1.0\"]", "97"),
            ("inherited[0]", "2"),
            ("inherited[\"01\"]", "98"),
            ("shadowed[\"-1\"]", "44"),
            ("boxed[2]", "'past string'"),
            ("boxedInherited[2]", "'past string'"),
            ("boxedShadowed[0]", "45"),
        ] {
            let (actual, _) = presented(&session, expression).await;
            if actual != expected {
                lies.push(format!("{entry}: {expression} is {actual}, not {expected}"));
            }
        }
        for expression in ["boxed[0]", "boxedInherited[1]"] {
            let (actual, _) = presented(&session, expression).await;
            if !actual.contains("string wrapper's character") {
                lies.push(format!(
                    "{entry}: {expression} is {actual}, not a character refusal"
                ));
            }
        }
        for key in ["-1", "-0", "1.5", "NaN", "Infinity", "4294967295"] {
            for object in ["typed", "inherited"] {
                let expression = format!("{object}[\"{key}\"]");
                let (actual, _) = presented(&session, &expression).await;
                if actual != "<error: the value has no such property>" {
                    lies.push(format!("{entry}: {expression} is {actual}, not absent"));
                }
            }
        }
        session.shutdown().await;
    }
    assert!(lies.is_empty(), "{}", lies.join("\n"));
}

#[tokio::test]
async fn inspection_errors_refuse_replaced_stack_getters() {
    let mut lies = Vec::new();
    for entry in MATRIX {
        let mut session = NodeSession::launch(entry, "values.js", &[], &["inspection"]).await;
        let reported = threads(&session.stop_at("inspection").await);
        let printer = reported.iter().find(|t| t.printer).expect("the printer");
        select_function(&session, printer.native, "inspection").await;
        let (_, custom) = presented(&session, "customStack").await;
        if !custom.contains("stack getter is not V8's original") {
            lies.push(format!("{entry}: custom stack getter shows {custom}"));
        }
        let (_, native) = presented(&session, "nativeStack").await;
        if !native.starts_with("TypeError: native stack is still readable\n    at inspection (") {
            lies.push(format!("{entry}: original stack getter shows {native}"));
        }
        session.shutdown().await;
    }
    assert!(lies.is_empty(), "{}", lies.join("\n"));
}

#[tokio::test]
async fn inspection_depth_does_not_follow_hidden_property_chains() {
    for entry in MATRIX {
        let mut session = NodeSession::launch(entry, "values.js", &[], &["inspection"]).await;
        let reported = threads(&session.stop_at("inspection").await);
        let printer = reported.iter().find(|t| t.printer).expect("the printer");
        select_function(&session, printer.native, "inspection").await;
        let deep = printer
            .values
            .iter()
            .find(|v| v.name == "deep")
            .expect("deep");
        let actual = presented(&session, "deep").await;
        assert!(
            lies_about(entry, deep, &actual).is_empty(),
            "{entry}: {actual:?}"
        );
        let (proxy, _) = presented(&session, "proxies").await;
        assert!(
            proxy.contains("proxy chain is too long")
                || (crate::session::major(entry) < 26 && proxy.contains("a proxy of a proxy")),
            "{entry}: {proxy}"
        );
        session.shutdown().await;
    }
}

#[tokio::test]
async fn dense_elements_cross_the_enumeration_limit_without_losing_values() {
    let mut lies = Vec::new();
    for entry in MATRIX {
        let mut session = NodeSession::launch(entry, "values.js", &[], &["elements"]).await;
        let reported = threads(&session.stop_at("elements").await);
        let printer = reported.iter().find(|t| t.printer).expect("the printer");
        select_function(&session, printer.native, "elementBoundary").await;
        for value in &printer.values {
            let actual = presented(&session, &value.name).await;
            lies.extend(lies_about(entry, value, &actual));
        }
        for name in ["tagged", "doubles"] {
            let (actual, _) = presented(&session, name).await;
            if !actual.contains("elements exceeds the limit of 1048576") {
                lies.push(format!(
                    "{entry}: {name} has no explicit element limit: {actual}"
                ));
            }
        }
        session.shutdown().await;
    }
    assert!(lies.is_empty(), "{}", lies.join("\n"));
}

#[tokio::test]
async fn inspection_children_include_numeric_own_properties_in_pages() {
    use microscope::{ValueChildQuery, ValueChildRelationship, ValueChildren};
    for entry in MATRIX {
        let mut session = NodeSession::launch(entry, "values.js", &[], &["inspection"]).await;
        let reported = threads(&session.stop_at("inspection").await);
        let printer = reported.iter().find(|t| t.printer).expect("the printer");
        select_function(&session, printer.native, "inspection").await;
        let result = session
            .scenario
            .handle()
            .evaluate(&Expression::parse("keyed").unwrap())
            .await
            .unwrap();
        let Evaluation::Value { value, .. } = result else {
            panic!("{result:?}")
        };
        let VariableState::Available {
            presentation: Some(presentation),
            ..
        } = value.state
        else {
            panic!("{:?}", value.state)
        };
        let ValueChildren::Available(children) = &presentation.children else {
            panic!("no children")
        };
        assert_eq!(children.total(), 3);
        for limit in [1, 2] {
            let mut found = Vec::new();
            for offset in (0..3).step_by(limit as usize) {
                let page = session
                    .scenario
                    .operation(
                        "children",
                        session.scenario.handle().value_children(
                            std::sync::Arc::clone(children),
                            ValueChildQuery { offset, limit },
                        ),
                    )
                    .await;
                for child in page.children.iter() {
                    let ValueChildRelationship::Field { name } = &child.relationship else {
                        panic!("{:?}", child.relationship)
                    };
                    let VariableState::Available {
                        presentation: Some(value),
                        ..
                    } = &child.state
                    else {
                        panic!("{:?}", child.state)
                    };
                    found.push((name.to_string(), value.summary.to_string()));
                }
            }
            assert_eq!(
                found,
                [
                    ("0".into(), "'zero'".into()),
                    ("foo".into(), "'bar'".into()),
                    ("4294967295".into(), "42".into())
                ],
                "{entry}"
            );
        }
        session.shutdown().await;
    }
}

#[tokio::test]
async fn with_scope_refuses_crossed_names_but_keeps_inner_bindings() {
    for entry in MATRIX {
        let mut session = NodeSession::launch(entry, "values.js", &[], &["with"]).await;
        let reported = threads(&session.stop_at("with").await);
        let printer = reported.iter().find(|t| t.printer).expect("the printer");
        select_function(&session, printer.native, "scope").await;
        assert_eq!(presented(&session, "inner").await.0, "7", "{entry}");
        for name in ["outer", "globalThis", "console", "missing"] {
            let (outer, _) = presented(&session, name).await;
            assert!(outer.contains("with context"), "{entry}: {name}: {outer}");
        }
        session.shutdown().await;
    }
}
