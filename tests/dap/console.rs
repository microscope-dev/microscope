//! The debug console runs the command language of the terminal debugger.

use serde_json::{Value, json};

use crate::dap::{Configuration, Dap, Profile, fixture, line_of, source};

fn repl(dap: &mut Dap, frame: &Value, line: &str) -> String {
    dap.request(
        "evaluate",
        json!({"expression": line, "frameId": frame, "context": "repl"}),
    )["result"]
        .as_str()
        .expect("result")
        .to_owned()
}

#[test]
fn console_commands_see_the_focused_frame_and_cannot_run_the_program() {
    let path = source("c/variables.c");
    let mut dap = Dap::start("console");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("variables-gcc-o0"),
        json!({}),
        &Configuration {
            functions: vec!["pointer_target".to_owned()],
            ..Configuration::default()
        },
    );
    let stop = dap.stopped(started.mark);
    let trace = dap.request("stackTrace", json!({"threadId": stop.thread}));
    let frames = trace["stackFrames"].as_array().expect("frames").clone();

    // Commands run in the frame the client focuses.
    assert_eq!(
        repl(&mut dap, &frames[0]["id"], "print parameter"),
        "(int) parameter = 40"
    );
    assert_eq!(
        repl(&mut dap, &frames[1]["id"], "print single"),
        "(float) single = 1.25"
    );
    let source = repl(&mut dap, &frames[1]["id"], "list");
    assert!(source.contains("=> "), "{source}");
    assert!(
        source.contains(&format!(
            "{}",
            line_of(&path, "pointer_target(40, &pointer_parameter_value) +")
        )),
        "{source}"
    );

    // Lines that are no command are expressions.
    assert_eq!(dap.request("evaluate", json!({"expression": "*pointer_parameter", "frameId": frames[0]["id"], "context": "repl"}))["result"], "42");
    for command in [
        "continue",
        "next",
        "step",
        "run",
        "finish",
        "advance main",
        "quit",
    ] {
        let message = dap.request_error(
            "evaluate",
            json!({"expression": command, "frameId": frames[0]["id"], "context": "repl"}),
        );
        assert_eq!(
            message,
            format!(
                "`{}` is not available in the debug console; use the debugger's controls",
                command.split(' ').next().expect("a command")
            )
        );
    }

    assert!(
        dap.request_error(
            "evaluate",
            json!({"expression": "print nope", "frameId": frames[0]["id"], "context": "repl"})
        )
        .contains("nope")
    );

    dap.finish();
}

#[test]
fn breakpoints_made_in_the_console_are_announced_and_reported() {
    let mut dap = Dap::start("console breakpoints");
    let started = dap.launch(
        Profile::Neovim,
        &fixture("basic"),
        json!({"stopOnEntry": true}),
        &Configuration::default(),
    );
    let entry = dap.stopped(started.mark);
    let mark = dap.mark();
    let output = dap.request(
        "evaluate",
        json!({"expression": "break breakpoint_target", "context": "repl"}),
    );
    assert!(
        output["result"]
            .as_str()
            .is_some_and(|text| text.starts_with("breakpoint 1 set")),
        "{output}"
    );
    let new = dap.event(mark, "breakpoint", |body| body["reason"] == "new");
    assert_eq!(new["breakpoint"]["verified"], true);
    assert_eq!(
        new["breakpoint"]["line"],
        line_of(&source("c/basic.c"), "return microscope_value;")
    );
    let id = new["breakpoint"]["id"].clone();

    let resumed = dap.send("continue", json!({"threadId": entry.thread}));
    dap.success(resumed);
    let stop = dap.stopped(resumed.mark);
    assert_eq!(stop.body["hitBreakpointIds"], json!([id]));

    // A disabled breakpoint stays listed, unverified, saying why.
    let mark = dap.mark();
    dap.request(
        "evaluate",
        json!({"expression": "disable 1", "context": "repl"}),
    );
    let disabled = dap.event(mark, "breakpoint", |body| body["reason"] == "changed");
    assert_eq!(
        (
            &disabled["breakpoint"]["id"],
            &disabled["breakpoint"]["verified"],
            &disabled["breakpoint"]["message"]
        ),
        (
            &id,
            &json!(false),
            &json!("disabled; enable 1 in the debug console")
        )
    );

    let mark = dap.mark();
    dap.request(
        "evaluate",
        json!({"expression": "enable 1", "context": "repl"}),
    );
    let enabled = dap.event(mark, "breakpoint", |body| body["reason"] == "changed");
    assert_eq!(enabled["breakpoint"]["verified"], true);

    let mark = dap.mark();
    dap.request(
        "evaluate",
        json!({"expression": "delete 1", "context": "repl"}),
    );
    let removed = dap.event(mark, "breakpoint", |body| body["reason"] == "removed");
    assert_eq!(removed["breakpoint"]["id"], id);

    let resumed = dap.send("continue", json!({"threadId": stop.thread}));
    dap.success(resumed);
    assert_eq!(
        dap.event(resumed.mark, "exited", |_| true),
        json!({"exitCode": 0})
    );
    dap.finish();
}

#[test]
fn console_output_is_styled_only_for_clients_that_accept_styling() {
    for ansi in [false, true] {
        let mut dap = Dap::start(format!("ansi {ansi}"));
        let mark = dap.mark();
        dap.request(
            "initialize",
            json!({"adapterID": "microscope", "supportsANSIStyling": ansi}),
        );
        dap.event(mark, "initialized", |_| true);
        let launch = dap.send(
            "launch",
            json!({"program": fixture("basic"), "stopOnEntry": true}),
        );
        dap.request("configurationDone", Value::Null);
        dap.success(launch);
        let stop = dap.stopped(launch.mark);
        let frame =
            dap.request("stackTrace", json!({"threadId": stop.thread}))["stackFrames"][0]["id"]
                .clone();
        let output = repl(&mut dap, &frame, "bt");
        assert_eq!(output.contains('\u{1b}'), ansi, "{output:?}");
        dap.finish();
    }
}

#[test]
fn console_commands_print_what_the_cli_prints_at_the_same_stop() {
    let commands = [
        "backtrace",
        "info breakpoints",
        "print parameter",
        "list",
        "disassemble",
        "registers",
        "info signals",
    ];
    let mut dap = Dap::start("console parity");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("variables-gcc-o0"),
        json!({}),
        &Configuration {
            functions: vec!["pointer_target".to_owned()],
            ..Configuration::default()
        },
    );
    let stop = dap.stopped(started.mark);
    let trace = dap.request("stackTrace", json!({"threadId": stop.thread, "levels": 1}));
    let frame = trace["stackFrames"][0]["id"].clone();
    let console = commands.map(|command| repl(&mut dap, &frame, command));
    dap.finish();

    for (command, console) in commands.iter().zip(console) {
        // The console may always show Unicode, so the CLI is told to.
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_microscope"))
            .env(
                "MICROSCOPE_CONFIG",
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/support/settings/unicode.toml"
                ),
            )
            .arg(fixture("variables-gcc-o0"))
            .args([
                "--batch",
                "-e",
                "break pointer_target",
                "-e",
                "run",
                "-e",
                command,
            ])
            .output()
            .expect("run the CLI");
        let cli = String::from_utf8_lossy(&output.stdout);
        assert!(!console.trim().is_empty(), "{command} printed nothing");
        assert!(
            cli.contains(console.trim_end()),
            "{command} differs:\nconsole:\n{console}\ncli:\n{cli}"
        );
    }
}

/// Stops `command-names` where every local is set, and returns the stop
/// and its innermost frame.
fn stopped_in_names(dap: &mut Dap, profile: Profile) -> (crate::dap::Stopped, Value) {
    let path = source("c/command-names.c");
    let started = dap.launch(
        profile,
        &fixture("command-names"),
        json!({}),
        &Configuration {
            sources: vec![(path.clone(), vec![line_of(&path, "volatile int sink")])],
            ..Configuration::default()
        },
    );
    let stop = dap.stopped(started.mark);
    let frame =
        dap.request("stackTrace", json!({"threadId": stop.thread, "levels": 1}))["stackFrames"][0]
            .clone();

    (stop, frame)
}

#[test]
fn variables_named_like_commands_evaluate_in_the_console() {
    let mut dap = Dap::start("console names");
    let (_, frame) = stopped_in_names(&mut dap, Profile::VsCode);
    let id = &frame["id"];

    // A name the frame knows is the variable, even where a command or its
    // alias has the same name.
    for (line, value) in [
        ("x", "20"),
        ("n", "10"),
        ("list", "5"),
        ("p", "4"),
        ("x + 1", "21"),
        ("list * p", "20"),
        ("p/x", "0"),
        ("where->y", "2"),
    ] {
        assert_eq!(repl(&mut dap, id, line), value, "{line}");
    }

    let pointer = dap.request(
        "evaluate",
        json!({"expression": "where", "frameId": id, "context": "repl"}),
    );
    assert_ne!(pointer["variablesReference"], 0, "{pointer}");

    // A command the frame has no variable for is still the command, and a
    // line that is no expression is always one.
    assert!(repl(&mut dap, id, "bt").starts_with("#0 "));
    assert!(repl(&mut dap, id, "frame").contains("names"));
    assert_eq!(repl(&mut dap, id, "p/x list"), "(int) list = 0x5");
    assert_eq!(repl(&mut dap, id, "print/x x"), "(int) x = 0x14");
    assert_eq!(repl(&mut dap, id, "print x"), "(int) x = 20");

    // An expression's mistake points at the text it is about.
    let message = dap.request_error(
        "evaluate",
        json!({"expression": "x + missing", "frameId": id, "context": "repl"}),
    );
    assert!(message.contains("missing"), "{message}");
    assert!(message.contains("^^^^^^^"), "{message}");
    dap.finish();
}

#[test]
fn assignments_in_the_console_change_what_the_variables_view_shows() {
    let mut dap = Dap::start("console assignments");
    let (_, frame) = stopped_in_names(&mut dap, Profile::VsCode);
    let id = frame["id"].clone();
    let locals = |dap: &mut Dap| {
        let scopes = dap.request("scopes", json!({"frameId": id}))["scopes"].clone();
        let reference = scopes
            .as_array()
            .expect("scopes")
            .iter()
            .find(|scope| scope["name"] == "Locals")
            .expect("locals")["variablesReference"]
            .clone();
        dap.request("variables", json!({"variablesReference": reference}))["variables"]
            .as_array()
            .expect("variables")
            .iter()
            .map(|variable| {
                (
                    variable["name"].as_str().expect("name").to_owned(),
                    variable["value"].as_str().expect("value").to_owned(),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>()
    };

    assert_eq!(locals(&mut dap)["x"], "20");

    let mark = dap.mark();
    assert_eq!(repl(&mut dap, &id, "x = 7"), "7");
    assert_eq!(
        dap.event(mark, "invalidated", |_| true),
        json!({"areas": ["variables"]})
    );
    assert_eq!(locals(&mut dap)["x"], "7");

    let mark = dap.mark();
    assert_eq!(repl(&mut dap, &id, "set var list = 9"), "(int) list = 9");
    dap.event(mark, "invalidated", |_| true);
    assert_eq!(locals(&mut dap)["list"], "9");

    let hover = dap.request(
        "evaluate",
        json!({"expression": "list + x", "frameId": id, "context": "hover"}),
    );
    assert_eq!(hover["result"], "16");

    // Reading changes nothing, so it invalidates nothing.
    let mark = dap.mark();
    assert_eq!(repl(&mut dap, &id, "x"), "7");
    assert_eq!(repl(&mut dap, &id, "print list"), "(int) list = 9");
    dap.request("threads", Value::Null);
    assert!(dap.events(mark, "invalidated").is_empty());
    dap.finish();
}
