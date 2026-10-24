//! Node through its inspector, as an editor drives it: a breakpoint in a
//! `.js` file, a step, the frame's scopes by JavaScript's names, and an
//! exception the program throws.

use serde_json::{Value, json};

use crate::dap::{Configuration, Dap, Profile, Stopped, line_of, source};
use crate::support::node;

/// The line of the acceptance fixture marked `marker`.
fn line(marker: &str) -> u64 {
    line_of(&source("node/uat.js"), &format!("// {marker}"))
}

/// The frame VS Code focuses as the thread stops, as its
/// `Thread.getTopStackFrame` chooses it: the first with a source, and at a
/// stop for another reason than a breakpoint or a step, the first whose
/// hint, or else its source's, is neither `subtle` nor `deemphasize`. V8
/// pauses inside Node's and its own native frames.
fn top(dap: &mut Dap, stop: &Stopped) -> Value {
    let trace = dap.request(
        "stackTrace",
        json!({"threadId": stop.thread, "startFrame": 0, "levels": 64}),
    );
    let regardless = ["breakpoint", "step"].contains(&stop.reason.as_str());
    trace["stackFrames"]
        .as_array()
        .expect("frames")
        .iter()
        .find(|frame| {
            let hint = frame
                .get("presentationHint")
                .unwrap_or_else(|| &frame["source"]["presentationHint"]);
            frame["source"].is_object()
                && (regardless || (hint != "subtle" && hint != "deemphasize"))
        })
        .cloned()
        .unwrap_or_else(|| panic!("no frame has source to focus: {trace}"))
}

/// The frame's scopes, by name, and the variables of the one named `scope`.
fn scope(dap: &mut Dap, frame: &Value, scope: &str) -> (Vec<String>, Vec<Value>) {
    let scopes = dap.request("scopes", json!({"frameId": frame["id"]}));
    let scopes = scopes["scopes"].as_array().expect("scopes").clone();
    let names = scopes
        .iter()
        .filter_map(|found| found["name"].as_str().map(str::to_owned))
        .collect();
    let reference = scopes
        .iter()
        .find(|found| found["name"] == scope)
        .unwrap_or_else(|| panic!("no {scope} scope: {scopes:?}"))["variablesReference"]
        .clone();
    let variables = dap.request("variables", json!({"variablesReference": reference}));
    (
        names,
        variables["variables"]
            .as_array()
            .expect("variables")
            .clone(),
    )
}

/// The value of the variable `name` among `variables`.
fn value<'a>(variables: &'a [Value], name: &str) -> &'a Value {
    &variables
        .iter()
        .find(|variable| variable["name"] == name)
        .unwrap_or_else(|| panic!("no {name}: {variables:?}"))["value"]
}

#[test]
fn a_launched_node_program_breaks_steps_and_stops_where_it_throws() {
    let mut dap = Dap::start("node launch");
    let script = node::fixture("uat.js");
    let started = dap.launch(
        Profile::VsCode,
        &node::node("26"),
        json!({"args": [script]}),
        &Configuration {
            sources: vec![(script.clone(), vec![line("AREA_LINE")])],
            exceptions: Some(json!({"filters": []})),
            ..Configuration::default()
        },
    );

    // An editor lists JavaScript's filters before any program runs.
    let filters: Vec<&str> = started.capabilities["exceptionBreakpointFilters"]
        .as_array()
        .expect("filters")
        .iter()
        .filter_map(|filter| filter["filter"].as_str())
        .collect();
    for filter in ["javascript-thrown", "javascript-uncaught"] {
        assert!(filters.contains(&filter), "no {filter}: {filters:?}");
    }

    let stop = dap.stopped(started.mark);
    assert_eq!(stop.reason, "breakpoint", "{stop:?}");
    let frame = top(&mut dap, &stop);
    assert_eq!(frame["name"], "area", "{frame}");
    assert_eq!(frame["line"], line("AREA_LINE"), "{frame}");
    assert!(
        frame["source"]["path"]
            .as_str()
            .is_some_and(|path| path.ends_with("tests/fixtures/node/uat.js")),
        "{frame}"
    );
    // V8 pauses at the statement's first expression, `side * side`.
    let column = std::fs::read_to_string(&script)
        .expect("the fixture")
        .lines()
        .find(|text| text.contains("// AREA_LINE"))
        .and_then(|text| text.find("side * side"))
        .expect("the multiplication");
    assert_eq!(frame["column"], column + 1, "{frame}");

    let mark = dap.mark();
    dap.request("next", json!({"threadId": stop.thread}));
    let stop = dap.stopped(mark);
    assert_eq!(stop.reason, "step", "{stop:?}");
    let frame = top(&mut dap, &stop);
    assert_eq!(frame["name"], "area", "{frame}");
    assert_eq!(frame["line"], line("RETURN_LINE"), "{frame}");

    let (names, locals) = scope(&mut dap, &frame, "Locals");
    assert_eq!(value(&locals, "result"), "16", "{locals:?}");
    let (_, arguments) = scope(&mut dap, &frame, "Arguments");
    assert_eq!(value(&arguments, "side"), "4", "{arguments:?}");
    // At a pause every local is named, so none is left unnamed; the
    // script's top level declares no bindings, so there is no Module.
    assert_eq!(
        names,
        ["Arguments", "Locals", "Global", "Registers"],
        "{names:?}"
    );

    dap.request(
        "setExceptionBreakpoints",
        json!({"filters": ["javascript-thrown"]}),
    );
    let mark = dap.mark();
    dap.request("continue", json!({"threadId": stop.thread}));
    let stop = dap.stopped(mark);
    assert_eq!(stop.reason, "exception", "{stop:?}");
    let frame = top(&mut dap, &stop);
    assert_eq!(frame["name"], "fail", "{frame}");
    assert_eq!(frame["line"], line("THROW_LINE"), "{frame}");
    let info = dap.request("exceptionInfo", json!({"threadId": stop.thread}));
    assert!(
        info["description"]
            .as_str()
            .is_some_and(|text| text.contains("the shape is wrong")),
        "{info}"
    );

    dap.request("setExceptionBreakpoints", json!({"filters": []}));
    let mark = dap.mark();
    dap.request("continue", json!({"threadId": stop.thread}));
    let exited = dap.event(mark, "exited", |_| true);
    assert_eq!(exited["exitCode"], 0, "{exited}");
    dap.output_containing(started.mark, "stdout", "DONE");
    dap.finish();
}
