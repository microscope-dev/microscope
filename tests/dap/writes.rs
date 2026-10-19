//! Changing variables, expressions, and memory from the client.

use serde_json::{Value, json};

use crate::dap::{Configuration, Dap, Profile, fixture, line_of, source};

/// Stops the variables program on the line with `marker`, with no
/// breakpoints left, and returns its frames.
fn stopped(dap: &mut Dap, marker: &str) -> (i64, Vec<Value>) {
    let path = source("c/variables.c");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("variables-gcc-o0"),
        json!({}),
        &Configuration {
            sources: vec![(path.clone(), vec![line_of(&path, marker)])],
            ..Configuration::default()
        },
    );
    let stop = dap.stopped(started.mark);
    dap.request(
        "setBreakpoints",
        json!({"source": {"path": path}, "breakpoints": []}),
    );
    let trace = dap.request("stackTrace", json!({"threadId": stop.thread}));

    (
        stop.thread,
        trace["stackFrames"].as_array().expect("frames").clone(),
    )
}

fn scope(dap: &mut Dap, frame: &Value, name: &str) -> Value {
    dap.request("scopes", json!({"frameId": frame["id"]}))["scopes"]
        .as_array()
        .expect("scopes")
        .iter()
        .find(|scope| scope["name"] == name)
        .expect("scope")["variablesReference"]
        .clone()
}

fn row(dap: &mut Dap, reference: &Value, name: &str) -> Value {
    dap.request("variables", json!({"variablesReference": reference}))["variables"]
        .as_array()
        .expect("variables")
        .iter()
        .find(|variable| variable["name"] == name)
        .unwrap_or_else(|| panic!("no {name}"))
        .clone()
}

#[test]
fn set_variable_changes_values_the_program_then_uses() {
    let mut dap = Dap::start("set variable");
    let (thread, frames) = stopped(&mut dap, "return **pointer_pointer");
    let locals = scope(&mut dap, &frames[0], "Locals");
    let pointee = row(&mut dap, &locals, "pointee");

    // Values with storage are editable; aggregates are not.
    assert_eq!(pointee["presentationHint"]["attributes"], json!([]));
    let pair = row(&mut dap, &locals, "pair");
    assert_eq!(pair["presentationHint"]["attributes"], json!(["readOnly"]));

    let mark = dap.mark();
    let set = dap.request(
        "setVariable",
        json!({"variablesReference": locals, "name": "pointee", "value": "pointee + 58"}),
    );
    assert_eq!(
        (&set["value"], &set["type"]),
        (&json!("100"), &json!("int"))
    );

    // The client is told to read values again.
    dap.event(mark, "invalidated", |body| {
        body["areas"] == json!(["variables"])
    });
    let through = dap.request(
        "evaluate",
        json!({"expression": "*pointer", "frameId": frames[0]["id"], "context": "watch"}),
    );
    assert_eq!(through["result"], "100");

    // Expressions can be set too, here in the caller's frame.
    let caller = dap.request(
        "setExpression",
        json!({"expression": "signed_int", "value": "5", "frameId": frames[1]["id"]}),
    );
    assert_eq!(caller["value"], "5");
    let main_locals = scope(&mut dap, &frames[1], "Locals");
    let narrow = dap.request_error(
        "setVariable",
        json!({"variablesReference": main_locals, "name": "unsigned_character", "value": "300"}),
    );
    assert_eq!(narrow, "300 does not fit `unsigned char` exactly");
    let record = dap.request_error(
        "setVariable",
        json!({"variablesReference": locals, "name": "pair", "value": "1"}),
    );
    assert_eq!(
        record,
        "`pair` cannot be assigned; only numbers, truth values, and pointers can; its type is `pointer_pair`"
    );

    // main checks the values it set, and now finds them changed.
    let resumed = dap.send("continue", json!({"threadId": thread}));
    dap.success(resumed);
    assert_eq!(
        dap.event(resumed.mark, "exited", |_| true),
        json!({"exitCode": 1})
    );
    dap.finish();
}

#[test]
fn write_memory_writes_bytes_and_reports_the_change() {
    let mut dap = Dap::start("write memory");
    let (_, frames) = stopped(&mut dap, "return **pointer_pointer");
    let locals = scope(&mut dap, &frames[0], "Locals");
    let pointee = row(&mut dap, &locals, "pointee");
    let reference = pointee["memoryReference"].clone();
    let mark = dap.mark();

    // 7, little-endian.
    let written = dap.request(
        "writeMemory",
        json!({"memoryReference": reference, "data": "BwAAAA=="}),
    );
    assert_eq!(written["bytesWritten"], 4);
    let event = dap.event(mark, "memory", |_| true);
    assert_eq!(
        (&event["memoryReference"], &event["count"]),
        (&reference, &json!(4))
    );
    assert_eq!(row(&mut dap, &locals, "pointee")["value"], "7");
    let read = dap.request(
        "readMemory",
        json!({"memoryReference": reference, "count": 4}),
    );
    assert_eq!(read["data"], "BwAAAA==");
    assert_eq!(
        dap.request_error(
            "writeMemory",
            json!({"memoryReference": reference, "data": "not base64!"})
        ),
        "the data is not base64"
    );
    assert_eq!(
        dap.request_error(
            "writeMemory",
            json!({"memoryReference": "0x8", "data": "AA=="})
        ),
        "memory at 0x8 cannot be written"
    );
    dap.finish();
}

/// Stops the jump program at `checked`'s first line, with no breakpoints
/// left, and returns the stopped thread.
fn stopped_in_checked(dap: &mut Dap) -> i64 {
    let path = source("c/jump.c");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("jump"),
        json!({}),
        &Configuration {
            sources: vec![(path.clone(), vec![line_of(&path, "jump: start")])],
            ..Configuration::default()
        },
    );
    let stop = dap.stopped(started.mark);
    dap.request(
        "setBreakpoints",
        json!({"source": {"path": path}, "breakpoints": []}),
    );

    stop.thread
}

fn top_line(dap: &mut Dap, thread: i64) -> Value {
    dap.request("stackTrace", json!({"threadId": thread}))["stackFrames"][0]["line"].clone()
}

/// Jump to Cursor: `gotoTargets` names a line's code, and `goto` moves the
/// thread there without running it, then stops it there as `goto`.
#[test]
fn goto_moves_a_thread_to_a_line_of_its_function() {
    let mut dap = Dap::start("goto");
    let thread = stopped_in_checked(&mut dap);
    let path = source("c/jump.c");
    let target = line_of(&path, "jump: target");
    let targets = dap.request(
        "gotoTargets",
        json!({"source": {"path": path}, "line": target}),
    );
    let targets = targets["targets"].as_array().expect("targets").clone();
    assert_eq!(targets.len(), 1, "{targets:?}");
    assert_eq!(targets[0]["line"], json!(target));
    let mark = dap.mark();
    dap.request(
        "goto",
        json!({"threadId": thread, "targetId": targets[0]["id"]}),
    );
    let stop = dap.stopped(mark);
    assert_eq!(stop.reason, "goto");
    assert_eq!(top_line(&mut dap, thread), json!(target));

    // A target belongs to the stop that named it.
    let stale = dap.request_error(
        "goto",
        json!({"threadId": thread, "targetId": targets[0]["id"]}),
    );
    assert_eq!(stale, "the goto target belongs to an earlier stop");

    // A line of another function is a target, but not for this thread.
    let call = line_of(&path, "jump: call");
    let elsewhere = dap.request(
        "gotoTargets",
        json!({"source": {"path": path}, "line": call}),
    );
    let refused = dap.request_error(
        "goto",
        json!({"threadId": thread, "targetId": elsewhere["targets"][0]["id"]}),
    );
    assert!(
        refused.contains("has no code in the function the thread is stopped in"),
        "{refused}"
    );

    // `status = value` and `status += 10` never ran.
    let resumed = dap.send("continue", json!({"threadId": thread}));
    dap.success(resumed);
    assert_eq!(
        dap.event(resumed.mark, "exited", |_| true),
        json!({"exitCode": 100})
    );
    dap.finish();
}

/// The innermost frame's registers can be set, and setting the program
/// counter moves the thread as `goto` does; a caller's cannot.
#[test]
fn registers_of_the_innermost_frame_can_be_set() {
    let mut dap = Dap::start("set registers");
    let thread = stopped_in_checked(&mut dap);
    let frames = dap.request("stackTrace", json!({"threadId": thread}))["stackFrames"]
        .as_array()
        .expect("frames")
        .clone();
    let caller = scope(&mut dap, &frames[1], "Registers");
    let caller_rbx = row(&mut dap, &caller, "rbx");
    assert_eq!(
        caller_rbx["presentationHint"]["attributes"],
        json!(["readOnly"])
    );
    let registers = scope(&mut dap, &frames[0], "Registers");
    let rax = row(&mut dap, &registers, "rax");
    assert_eq!(rax["presentationHint"]["attributes"], json!([]));
    assert_eq!(rax["evaluateName"], "$rax");
    let set = dap.request(
        "setVariable",
        json!({"variablesReference": registers, "name": "rax", "value": "0x2a"}),
    );
    assert_eq!(set["value"], "0x000000000000002a");

    // Setting the program counter publishes the stop again, even where
    // the thread already is.
    let rip = row(&mut dap, &registers, "rip");
    let mark = dap.mark();
    dap.request(
        "setVariable",
        json!({"variablesReference": registers, "name": "rip", "value": rip["value"]}),
    );
    assert_eq!(dap.stopped(mark).reason, "goto");

    // The thread runs on from where it was; checked computes its own rax.
    let resumed = dap.send("continue", json!({"threadId": thread}));
    dap.success(resumed);
    assert_eq!(
        dap.event(resumed.mark, "exited", |_| true),
        json!({"exitCode": 111})
    );
    dap.finish();
}
