//! Source, function, and hit-count breakpoints as clients set them.

use serde_json::{Value, json};

use crate::dap::{Configuration, Dap, Profile, breakpoints, fixture, line_of, source};

fn set_lines(dap: &mut Dap, path: &std::path::Path, lines: &[u64]) -> Vec<Value> {
    breakpoints(&dap.request(
        "setBreakpoints",
        json!({
            "source": {"path": path},
            "breakpoints": lines.iter().map(|line| json!({"line": line})).collect::<Vec<_>>(),
        }),
    ))
}

#[test]
fn source_breakpoints_move_to_code_keep_their_ids_and_explain_failures() {
    let path = source("c/line-sliding.c");
    let comment = line_of(&path, "// a comment inside the function");
    let code = line_of(&path, "sliding_sink = doubled;");
    let between = line_of(&path, "// a comment between the functions");
    let mut dap = Dap::start("source breakpoints");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("line-sliding"),
        json!({"stopOnEntry": true}),
        &Configuration::default(),
    );
    let entry = dap.stopped(started.mark);

    let first = set_lines(&mut dap, &path, &[comment, between]);
    assert_eq!(first[0]["verified"], true);
    assert_eq!(
        first[0]["line"], code,
        "a line without code moves to the next"
    );
    assert_eq!(first[1]["verified"], false);
    assert_eq!(first[1]["reason"], "failed");
    assert_eq!(first[1]["line"], between);
    assert!(
        first[1]["message"]
            .as_str()
            .is_some_and(|message| message.contains("no code")),
        "{first:?}"
    );

    // Sending the same breakpoints again keeps their ids.
    let again = set_lines(&mut dap, &path, &[comment, between]);
    assert_eq!(again, first);

    // Sending the moved line, as dape does until lines settle, is stable.
    let settled = set_lines(&mut dap, &path, &[code]);
    assert_eq!(settled[0]["line"], code);
    assert_eq!(set_lines(&mut dap, &path, &[code]), settled);

    let resumed = dap.send("continue", json!({"threadId": entry.thread}));
    dap.success(resumed);
    let stop = dap.stopped(resumed.mark);
    assert_eq!(stop.body["hitBreakpointIds"], json!([settled[0]["id"]]));
    let frames = dap.inspect_as(Profile::VsCode, &stop);
    assert_eq!(
        (frames[0]["name"].clone(), frames[0]["line"].clone()),
        (json!("first_function"), json!(code))
    );

    // Clearing the file's breakpoints lets the program finish.
    assert!(set_lines(&mut dap, &path, &[]).is_empty());
    let resumed = dap.send("continue", json!({"threadId": stop.thread}));
    dap.success(resumed);
    assert_eq!(
        dap.event(resumed.mark, "exited", |_| true),
        json!({"exitCode": 0})
    );
    dap.finish();
}

#[test]
fn function_breakpoints_stop_at_every_function_with_the_name() {
    let mut dap = Dap::start("function breakpoints");
    let started = dap.launch(
        Profile::Neovim,
        &fixture("same-names"),
        json!({}),
        &Configuration {
            functions: vec![
                "helper".to_owned(),
                "no_such_function".to_owned(),
                "other.c:".to_owned(),
            ],
            ..Configuration::default()
        },
    );
    let [helper, missing, malformed] = &started.function_breakpoints[..] else {
        panic!("three breakpoints");
    };

    assert_eq!(helper["verified"], true);

    // A function no loaded module has waits for one that does.
    assert_eq!(
        (&missing["verified"], &missing["reason"]),
        (&json!(false), &json!("pending"))
    );
    assert_eq!(
        missing["message"],
        "no loaded module has code for no_such_function; the breakpoint resolves when one that does loads"
    );
    assert_eq!(
        malformed["message"],
        "'other.c:' is not a function, 0xaddress, file:line, or file:function"
    );
    let mut files = Vec::new();
    let mut mark = started.mark;

    for _ in 0..2 {
        let stop = dap.stopped(mark);
        assert_eq!(stop.reason, "function breakpoint");
        assert_eq!(stop.body["hitBreakpointIds"], json!([helper["id"]]));
        let frames = dap.inspect_as(Profile::Neovim, &stop);
        assert_eq!(frames[0]["name"], "helper");
        files.push(frames[0]["source"]["name"].clone());
        let resumed = dap.send("continue", json!({"threadId": stop.thread}));
        dap.success(resumed);
        mark = resumed.mark;
    }

    files.sort_by_key(ToString::to_string);
    assert_eq!(files, [json!("main.c"), json!("other.c")]);
    assert_eq!(dap.event(mark, "exited", |_| true), json!({"exitCode": 0}));
    dap.finish();
}

#[test]
fn hit_conditions_choose_which_hits_stop_and_reject_ambiguous_counts() {
    let mut dap = Dap::start("hit conditions");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("hit-counts-gcc-o0"),
        json!({"stopOnEntry": true}),
        &Configuration::default(),
    );
    let entry = dap.stopped(started.mark);
    let set = dap.request(
        "setFunctionBreakpoints",
        json!({"breakpoints": [
            {"name": "counted", "hitCondition": ">=38"},
            {"name": "caller", "hitCondition": "5"},
            {"name": "caller", "hitCondition": "%0"},
        ]}),
    );
    let [counted, bare, zero] = &breakpoints(&set)[..] else {
        panic!("three breakpoints");
    };

    assert_eq!(counted["verified"], true);
    assert_eq!(
        bare["message"],
        "invalid hit condition: a bare count is ambiguous; write ==5 to stop only at that hit or >=5 to stop at it and every later hit"
    );
    assert_eq!(
        zero["message"],
        "invalid hit condition: no hit can satisfy %0"
    );

    let mut mark = dap.send("continue", json!({"threadId": entry.thread})).mark;
    for call in 38..=40 {
        let stop = dap.stopped(mark);
        assert_eq!(stop.body["hitBreakpointIds"], json!([counted["id"]]));
        let trace = dap.request("stackTrace", json!({"threadId": stop.thread}));
        let frame = trace["stackFrames"][0]["id"].clone();
        let value = dap.request(
            "evaluate",
            json!({"expression": "call", "frameId": frame, "context": "watch"}),
        );
        assert_eq!(value["result"], call.to_string());
        let resumed = dap.send("continue", json!({"threadId": stop.thread}));
        dap.success(resumed);
        mark = resumed.mark;
    }

    assert_eq!(dap.event(mark, "exited", |_| true), json!({"exitCode": 0}));
    dap.finish();
}

#[test]
fn breakpoints_sharing_an_address_are_reported_by_one_stop() {
    let path = source("c/basic.c");
    let line = line_of(&path, "return microscope_value;");
    let mut dap = Dap::start("shared address");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("basic"),
        json!({}),
        &Configuration {
            sources: vec![(path, vec![line])],
            functions: vec!["breakpoint_target".to_owned()],
            ..Configuration::default()
        },
    );
    let source_id = started.source_breakpoints[0][0]["id"].clone();
    let function_id = started.function_breakpoints[0]["id"].clone();
    let stop = dap.stopped(started.mark);

    // Two kinds of breakpoint hit together make a plain breakpoint stop.
    assert_eq!(stop.reason, "breakpoint");
    let mut ids = stop.body["hitBreakpointIds"]
        .as_array()
        .expect("ids")
        .clone();
    ids.sort_by_key(Value::as_i64);
    assert_eq!(ids, [source_id.clone(), function_id]);

    // Removing one leaves the other installed.
    dap.request("setFunctionBreakpoints", json!({"breakpoints": []}));
    let resumed = dap.send("continue", json!({"threadId": stop.thread}));
    dap.success(resumed);
    let again = dap.stopped(resumed.mark);
    assert_eq!(again.body["hitBreakpointIds"], json!([source_id]));
    dap.finish();
}

#[test]
fn breakpoints_set_while_running_stop_it_and_cleared_ones_never_do() {
    let mut dap = Dap::start("edits while running");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("hot-calls"),
        json!({}),
        &Configuration::default(),
    );
    dap.event(started.mark, "process", |_| true);

    for _ in 0..10 {
        // Four threads call the function as fast as they can.
        let set = dap.send(
            "setFunctionBreakpoints",
            json!({"breakpoints": [{"name": "hot_function"}]}),
        );
        let id = breakpoints(&dap.success(set))[0]["id"].clone();
        let stop = dap.stopped(set.mark);
        assert_eq!(stop.body["hitBreakpointIds"], json!([id]));
        dap.inspect_as(Profile::VsCode, &stop);
        dap.request("setFunctionBreakpoints", json!({"breakpoints": []}));
        let resumed = dap.send("continue", json!({"threadId": stop.thread}));
        dap.success(resumed);

        // With the breakpoint gone, the next stop is the pause.
        let paused = dap.send("pause", json!({"threadId": stop.thread}));
        dap.success(paused);
        assert_eq!(dap.stopped(resumed.mark).reason, "pause");
        let resumed = dap.send("continue", json!({"threadId": stop.thread}));
        dap.success(resumed);
    }

    dap.finish();
}

#[test]
fn breakpoints_in_files_no_loaded_module_has_wait_pending() {
    let mut dap = Dap::start("unrelated files");
    let started = dap.launch(
        Profile::Helix,
        &fixture("basic"),
        json!({"stopOnEntry": true}),
        &Configuration {
            sources: vec![
                ("/nonexistent/unrelated.c".into(), vec![3]),
                (source("c/spin.c"), vec![10]),
            ],
            ..Configuration::default()
        },
    );
    for breakpoints in &started.source_breakpoints {
        assert_eq!(breakpoints[0]["verified"], false);
        assert_eq!(breakpoints[0]["reason"], "pending");
        assert!(
            breakpoints[0]["message"]
                .as_str()
                .is_some_and(|message| message.starts_with("no loaded module has code for "))
        );
    }

    dap.stopped(started.mark);
    dap.finish();
}

/// Launches the hit-count program stopped at its entry.
fn counting(dap: &mut Dap) -> crate::dap::Stopped {
    let started = dap.launch(
        Profile::VsCode,
        &fixture("hit-counts-gcc-o0"),
        json!({"stopOnEntry": true}),
        &Configuration::default(),
    );
    dap.stopped(started.mark)
}

/// The `call` argument of the innermost frame of a stopped thread.
fn call_of(dap: &mut Dap, thread: i64) -> String {
    let trace = dap.request("stackTrace", json!({"threadId": thread, "levels": 1}));
    let frame = trace["stackFrames"][0]["id"].clone();
    dap.request(
        "evaluate",
        json!({"expression": "call", "frameId": frame, "context": "watch"}),
    )["result"]
        .as_str()
        .expect("result")
        .to_owned()
}

#[test]
fn conditions_stop_only_where_they_hold() {
    let mut dap = Dap::start("conditions");
    let entry = counting(&mut dap);
    let set = dap.request(
        "setFunctionBreakpoints",
        json!({"breakpoints": [
            {"name": "counted", "condition": "call % 10 == 0 || (call > 37 && last_call == call - 1)"},
            {"name": "caller", "condition": "call = 3"},
        ]}),
    );
    let [counted, invalid] = &breakpoints(&set)[..] else {
        panic!("two breakpoints");
    };

    assert_eq!(counted["verified"], true);
    assert_eq!(invalid["verified"], false);
    assert_eq!(
        invalid["message"],
        "invalid condition: conditions and log messages cannot assign; compare with `==`"
    );

    let mut mark = dap.send("continue", json!({"threadId": entry.thread})).mark;

    for expected in ["10", "20", "30", "38", "39", "40"] {
        let stop = dap.stopped(mark);
        assert_eq!(stop.body["hitBreakpointIds"], json!([counted["id"]]));
        assert_eq!(call_of(&mut dap, stop.thread), expected);
        let resumed = dap.send("continue", json!({"threadId": stop.thread}));
        dap.success(resumed);
        mark = resumed.mark;
    }

    assert_eq!(dap.event(mark, "exited", |_| true), json!({"exitCode": 0}));
    dap.finish();
}

#[test]
fn logpoints_log_values_instead_of_stopping() {
    let path = source("c/hit-counts.c");
    let line = line_of(&path, "last_call = call;");
    let mut dap = Dap::start("logpoints");
    let entry = counting(&mut dap);
    let set = dap.request(
        "setBreakpoints",
        json!({"source": {"path": path}, "breakpoints": [
            {"line": line, "logMessage": "call {call} after {last_call}, {{braces}}"},
            {"line": line, "logMessage": "seven", "condition": "call == 7"},
            {"line": line, "logMessage": "bad {call +}"},
        ]}),
    );
    let set = breakpoints(&set);
    assert_eq!(
        (&set[0]["verified"], &set[1]["verified"]),
        (&json!(true), &json!(true))
    );
    assert_eq!(
        set[2]["message"],
        "invalid log message: expected an operand, found the end of the expression"
    );
    let resumed = dap.send("continue", json!({"threadId": entry.thread}));
    dap.success(resumed);
    assert_eq!(
        dap.event(resumed.mark, "exited", |_| true),
        json!({"exitCode": 0})
    );
    let console = dap.output_text(resumed.mark, "console");
    let expected = (1..=40)
        .map(|call| {
            let mut line = format!("call {call} after {}, {{braces}}\n", call - 1);
            if call == 7 {
                line.push_str("seven\n");
            }

            line
        })
        .collect::<String>();

    // The two logpoints share one address, so their order within a hit
    // follows their ids.
    assert_eq!(console, expected);
    dap.finish();
}

#[test]
fn a_condition_that_cannot_be_evaluated_stops_and_says_why() {
    let mut dap = Dap::start("condition errors");
    let entry = counting(&mut dap);
    let set = dap.request(
        "setFunctionBreakpoints",
        json!({"breakpoints": [{"name": "counted", "condition": "no_such_value > 1"}]}),
    );
    let id = breakpoints(&set)[0]["id"].clone();
    let resumed = dap.send("continue", json!({"threadId": entry.thread}));
    dap.success(resumed);
    let stop = dap.stopped(resumed.mark);
    assert_eq!(stop.body["hitBreakpointIds"], json!([id]));
    assert_eq!(call_of(&mut dap, stop.thread), "1");
    let important = dap.output_containing(resumed.mark, "important", "condition");
    assert_eq!(
        important,
        format!(
            "breakpoint {id} stopped because its condition could not be evaluated: \
             no variable is named `no_such_value` here\n"
        )
    );
    dap.finish();
}

#[test]
fn breakpoints_in_libraries_wait_for_them_and_follow_them_in_and_out() {
    let mut dap = Dap::start("library breakpoints");

    // The program loads a library, unloads it, stops in after_unload, and
    // loads it again. Without that stop, an unload and reload could pass
    // before the client hears of either, which leaves nothing to report.
    let started = dap.launch(
        Profile::VsCode,
        &fixture("globals-shared"),
        json!({}),
        &Configuration {
            functions: vec!["dso_touch".to_owned(), "after_unload".to_owned()],
            ..Configuration::default()
        },
    );
    let pending = &started.function_breakpoints[0];
    let id = pending["id"].clone();
    assert_eq!(
        (&pending["verified"], &pending["reason"]),
        (&json!(false), &json!("pending"))
    );
    let mut mark = started.mark;

    for round in 0..2 {
        // The library loads: the breakpoint resolves, then stops in it.
        let changed = dap.event(mark, "breakpoint", |body| {
            body["breakpoint"]["id"] == id && body["breakpoint"]["verified"] == true
        });
        assert_eq!(changed["reason"], "changed");
        assert!(
            changed["breakpoint"]["source"]["path"]
                .as_str()
                .is_some_and(|path| path.ends_with("shared/library.c")),
            "{changed}"
        );
        let stop = dap.stopped(mark);
        assert_eq!(stop.body["hitBreakpointIds"], json!([id]), "round {round}");
        let trace = dap.request("stackTrace", json!({"threadId": stop.thread, "levels": 1}));
        assert_eq!(trace["stackFrames"][0]["name"], "dso_touch");
        let resumed = dap.send("continue", json!({"threadId": stop.thread}));
        dap.success(resumed);

        // The library unloads: the breakpoint waits again, as the client
        // hears before the next stop or the exit.
        let (next, body) = dap.next_event(resumed.mark, &["stopped", "exited"]);
        let messages = dap.messages_since(resumed.mark);
        let pending = messages.iter().position(|message| {
            message["event"] == "breakpoint"
                && message["body"]["breakpoint"]["id"] == id
                && message["body"]["breakpoint"]["reason"] == "pending"
        });
        let ended = messages
            .iter()
            .position(|message| message["event"] == next.as_str());
        assert!(
            pending.is_some_and(|pending| Some(pending) < ended),
            "{messages:?}"
        );

        if round == 0 {
            assert_eq!(
                (next.as_str(), &body["reason"]),
                ("stopped", &json!("function breakpoint"))
            );
            let resumed = dap.send("continue", json!({"threadId": body["threadId"]}));
            dap.success(resumed);
            mark = resumed.mark;
        } else {
            assert_eq!((next.as_str(), body), ("exited", json!({"exitCode": 0})));
        }
    }

    dap.finish();
}

/// Continues a program to its end, returning each stop's innermost frame
/// and the name of the frame it was inlined into or called from.
fn stops_until_exit(dap: &mut Dap, mut mark: crate::dap::Mark) -> Vec<(String, i64, String)> {
    let mut stops = Vec::new();
    loop {
        let (kind, body) = dap.next_event(mark, &["stopped", "exited"]);
        if kind == "exited" {
            return stops;
        }

        let thread = body["threadId"].as_i64().expect("threadId");
        let trace = dap.request("stackTrace", json!({"threadId": thread, "levels": 2}));
        let name = |index: usize| {
            trace["stackFrames"][index]["name"]
                .as_str()
                .expect("name")
                .trim_end_matches(" [inlined]")
                .to_owned()
        };

        stops.push((
            name(0),
            trace["stackFrames"][0]["line"].as_i64().expect("line"),
            name(1),
        ));
        let resumed = dap.send("continue", json!({"threadId": thread}));
        dap.success(resumed);
        mark = resumed.mark;
    }
}

#[test]
fn breakpoints_in_inlined_code_stop_in_every_inlined_copy_as_gdb_does() {
    let path = source("c/inline.c");
    let line = line_of(&path, "inline_sink = incremented;");

    // gdb stops at the line as often in each build: gcc's line table marks
    // a statement there in only some of the inlined copies.
    for (program, line_callers) in [
        ("inline-gcc-o2", vec!["middle", "caller", "branchy"]),
        (
            "inline-clang-o2",
            vec!["middle", "middle", "caller", "caller", "branchy"],
        ),
    ] {
        // A function breakpoint stops where each copy begins.
        let mut dap = Dap::start(format!("{program} function"));
        let started = dap.launch(
            Profile::Neovim,
            &fixture(program),
            json!({}),
            &Configuration {
                functions: vec!["leaf".to_owned()],
                ..Configuration::default()
            },
        );
        assert_eq!(started.function_breakpoints[0]["verified"], true);
        let stops = stops_until_exit(&mut dap, started.mark);
        assert!(stops.iter().all(|(name, ..)| name == "leaf"), "{stops:?}");
        assert_eq!(
            stops
                .iter()
                .map(|(.., caller)| caller.as_str())
                .collect::<Vec<_>>(),
            ["middle", "middle", "caller", "caller", "branchy"]
        );
        dap.finish();

        // A line in the inlined body stops in the inlined frame.
        let mut dap = Dap::start(format!("{program} line"));
        let started = dap.launch(
            Profile::Neovim,
            &fixture(program),
            json!({}),
            &Configuration {
                sources: vec![(path.clone(), vec![line])],
                ..Configuration::default()
            },
        );
        let stops = stops_until_exit(&mut dap, started.mark);
        let line = i64::try_from(line).expect("line");
        assert!(
            stops
                .iter()
                .all(|(name, stopped, _)| name == "leaf" && *stopped == line),
            "{stops:?}"
        );
        assert_eq!(
            stops
                .iter()
                .map(|(.., caller)| caller.as_str())
                .collect::<Vec<_>>(),
            line_callers
        );
        dap.finish();
    }
}

#[test]
fn a_function_breakpoint_stops_in_every_overload_and_method_of_the_name() {
    let mut dap = Dap::start("overloads");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("overloads-cpp-gcc-o0"),
        json!({}),
        &Configuration {
            functions: vec!["pick".to_owned()],
            ..Configuration::default()
        },
    );
    assert_eq!(started.function_breakpoints[0]["verified"], true);

    // Each overload and the method, by their lines; frames name functions
    // as their debug information does, without scope or parameters.
    let path = source("cpp/overloads.cpp");
    let stops = stops_until_exit(&mut dap, started.mark);
    let expected = [
        "int pick(int value)",
        "double pick(double value)",
        "int pick() const",
    ]
    .map(|marker| {
        (
            "pick".to_owned(),
            i64::try_from(line_of(&path, marker)).expect("line"),
            "main".to_owned(),
        )
    });
    assert_eq!(stops, expected);
    dap.finish();
}

/// Function breakpoints find code without debug information by its
/// symbols: an indirect function stops in the implementation its resolver
/// chose, which the stack names.
#[test]
fn function_breakpoints_find_code_without_debug_information_by_its_symbols() {
    let mut dap = Dap::start("symbol breakpoints");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("measure-gcc-nodebug"),
        json!({"stopOnEntry": true}),
        &Configuration::default(),
    );
    let entry = dap.stopped(started.mark);
    dap.request(
        "setFunctionBreakpoints",
        json!({"breakpoints": [{"name": "measure"}]}),
    );
    let resumed = dap.send("continue", json!({"threadId": entry.thread}));
    dap.success(resumed);
    let stop = dap.stopped(resumed.mark);
    let set = dap.request(
        "setFunctionBreakpoints",
        json!({"breakpoints": [{"name": "measure"}, {"name": "strlen"}]}),
    );
    let [_, strlen] = &breakpoints(&set)[..] else {
        panic!("two breakpoints: {set}");
    };

    assert_eq!(strlen["verified"], true, "{strlen}");
    assert!(strlen["instructionReference"].is_string(), "{strlen}");
    let resumed = dap.send("continue", json!({"threadId": stop.thread}));
    dap.success(resumed);
    let stop = dap.stopped(resumed.mark);
    assert_eq!(stop.body["hitBreakpointIds"], json!([strlen["id"]]));
    let trace = dap.request("stackTrace", json!({"threadId": stop.thread}));
    let names = trace["stackFrames"]
        .as_array()
        .expect("frames")
        .iter()
        .take(2)
        .map(|frame| frame["name"].as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>();
    assert!(
        names[0].starts_with("__strlen_") && names[1].starts_with("measure+"),
        "{names:?}"
    );

    dap.finish();
}
