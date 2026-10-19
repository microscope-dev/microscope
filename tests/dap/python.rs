//! Python through its agent, as an editor drives it: a breakpoint in a `.py`
//! file, steps, and an uncaught exception, launched and attached.

use serde_json::{Value, json};

use crate::dap::{Configuration, Dap, Mark, Profile, Stopped, line_of, source};
use crate::support::{ExternalProcess, python};

/// The line of the stepping fixture marked `marker`.
fn line(marker: &str) -> u64 {
    line_of(&source("python/stepping.py"), &format!("# {marker}"))
}

/// The breakpoint at `marker` and the uncaught Python exceptions, which no
/// other exception filter joins.
fn configuration(markers: &[&str]) -> Configuration {
    Configuration {
        sources: vec![(
            python::fixture("stepping.py"),
            markers.iter().map(|marker| line(marker)).collect(),
        )],
        exceptions: Some(json!({"filters": ["python-uncaught"]})),
        ..Configuration::default()
    }
}

/// The frame VS Code focuses as a thread stops, as its
/// `Thread.getTopStackFrame` chooses it: the first with a source. Only at a
/// stop for another reason than a breakpoint or a step does it pass over a
/// frame whose hint, or else its source's, is `subtle` or `deemphasize`. So
/// at a breakpoint or step the agent's and the interpreter's frames must
/// have no source.
fn focused(dap: &mut Dap, stop: &Stopped) -> Value {
    let trace = dap.request(
        "stackTrace",
        json!({"threadId": stop.thread, "startFrame": 0, "levels": 64}),
    );

    let regardless = ["breakpoint", "step", "function breakpoint"].contains(&stop.reason.as_str());
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

/// Checks the focused frame is `function` at the line marked `marker`.
fn assert_focused(dap: &mut Dap, stop: &Stopped, function: &str, marker: &str) -> Value {
    let frame = focused(dap, stop);
    assert_eq!(frame["name"], function, "at {marker}: {frame}");
    assert_eq!(frame["line"], line(marker), "at {marker}: {frame}");
    assert!(
        frame["source"]["path"]
            .as_str()
            .is_some_and(|path| path.ends_with("tests/fixtures/python/stepping.py")),
        "{frame}"
    );
    frame
}

/// Sends `command` for the stopped thread and waits for its stop.
fn resume(dap: &mut Dap, command: &str, stop: &Stopped) -> Stopped {
    let mark = dap.mark();
    dap.request(command, json!({"threadId": stop.thread}));
    dap.stopped(mark)
}

/// The value of `name` in the frame's scope `scope`.
fn variable(dap: &mut Dap, frame: &Value, scope: &str, name: &str) -> Value {
    let scopes = dap.request("scopes", json!({"frameId": frame["id"]}));
    let locals = scopes["scopes"]
        .as_array()
        .expect("scopes")
        .iter()
        .find(|found| found["name"] == scope)
        .unwrap_or_else(|| panic!("no {scope} scope: {scopes}"))["variablesReference"]
        .clone();

    let variables = dap.request("variables", json!({"variablesReference": locals}));
    variables["variables"]
        .as_array()
        .expect("variables")
        .iter()
        .find(|variable| variable["name"] == name)
        .unwrap_or_else(|| panic!("no local {name}: {variables}"))["value"]
        .clone()
}

/// Once confirmed, a breakpoint the agent holds stays verified: a step or
/// another change to the agent's configuration does not make it pending
/// again while the agent acknowledges it, or an editor would gray it out.
fn assert_verified_once_confirmed(dap: &mut Dap, since: Mark) {
    let changes = dap.events(since, "breakpoint");
    let first = changes
        .iter()
        .position(|change| change["breakpoint"]["verified"] == true)
        .unwrap_or_else(|| panic!("the breakpoint was never confirmed: {changes:#?}"));
    let unverified = changes[first..]
        .iter()
        .filter(|change| change["breakpoint"]["verified"] != true)
        .collect::<Vec<_>>();
    assert!(unverified.is_empty(), "{unverified:#?}");
}

#[test]
fn a_launched_python_program_breaks_steps_and_stops_at_an_uncaught_exception() {
    let mut dap = Dap::start("python launch");
    let started = dap.launch(
        Profile::VsCode,
        &python::interpreter("3.14"),
        json!({"args": [python::fixture("stepping.py"), "--editor"]}),
        &configuration(&["CALLER_CALL"]),
    );

    // Its code loads only as the interpreter runs the file.
    assert_eq!(started.source_breakpoints[0][0]["verified"], false);

    // The first stop is the breakpoint, not an exception the interpreter's
    // own C code raised and caught as it started.
    let stop = dap.stopped(started.mark);
    assert_eq!(stop.reason, "breakpoint", "{stop:?}");
    assert_eq!(stop.body["hitBreakpointIds"], json!([1]), "{stop:?}");
    let frame = assert_focused(&mut dap, &stop, "caller", "CALLER_CALL");
    assert_eq!(variable(&mut dap, &frame, "Locals", "start"), "1");

    let stop = resume(&mut dap, "stepIn", &stop);
    assert_eq!(stop.reason, "step", "{stop:?}");
    let frame = assert_focused(&mut dap, &stop, "callee", "CALLEE_FIRST");
    assert_eq!(variable(&mut dap, &frame, "Arguments", "value"), "1");
    let stop = resume(&mut dap, "stepOut", &stop);
    assert_focused(&mut dap, &stop, "caller", "CALLER_CALL");
    let stop = resume(&mut dap, "next", &stop);
    assert_focused(&mut dap, &stop, "caller", "CALLER_AFTER");

    // The worker's exception leaves Thread.run for threading.excepthook.
    let stop = resume(&mut dap, "continue", &stop);
    assert_eq!(stop.reason, "exception", "{stop:?}");
    assert_eq!(stop.body["text"], "unhandled exception", "{stop:?}");
    let raised = format!(
        "RuntimeError: worker failed, raised in worker at {}:{}",
        python::fixture("stepping.py").display(),
        line("WORKER_RAISE")
    );
    assert_eq!(stop.body["description"], raised, "{stop:?}");
    let info = dap.request("exceptionInfo", json!({"threadId": stop.thread}));
    assert_eq!(info["breakMode"], "unhandled", "{info}");
    assert_eq!(info["description"], raised, "{info}");
    let frame = focused(&mut dap, &stop);
    assert_eq!(frame["name"], "Thread.run", "{frame}");

    assert_verified_once_confirmed(&mut dap, started.mark);

    let mark = dap.mark();
    dap.request("continue", json!({"threadId": stop.thread}));
    let exited = dap.event(mark, "exited", |_| true);
    assert_eq!(exited["exitCode"], 0, "{exited}");
    dap.output_containing(started.mark, "stdout", "STEPPING DONE");
    dap.finish();
}

#[test]
fn an_attached_python_program_breaks_and_steps() {
    let mut process = ExternalProcess::spawn_command(
        std::process::Command::new(python::interpreter("3.15t"))
            .arg(python::fixture("stepping.py"))
            .args(["--editor", "--attach"]),
    );
    let mut dap = Dap::start("python attach");
    let started = dap.begin(
        Profile::VsCode,
        (
            "attach",
            json!({"pid": process.process_id().get().to_string()}),
        ),
        &configuration(&["CALLER_START"]),
    );

    // Until the program runs Python again, nothing can load the agent.
    let mark = dap.mark();
    process.release();
    let stop = dap.stopped(mark);
    assert_eq!(stop.reason, "breakpoint", "{stop:?}");
    assert_focused(&mut dap, &stop, "caller", "CALLER_START");
    let stop = resume(&mut dap, "next", &stop);
    assert_eq!(stop.reason, "step", "{stop:?}");
    let frame = assert_focused(&mut dap, &stop, "caller", "CALLER_CALL");
    assert_eq!(variable(&mut dap, &frame, "Locals", "start"), "1");
    assert_verified_once_confirmed(&mut dap, started.mark);

    // Detaching disarms the agent and leaves the program to finish alone;
    // its worker's exception goes to threading.excepthook unstopped.
    dap.finish();
    assert_eq!(process.wait().code(), Some(0));
}
