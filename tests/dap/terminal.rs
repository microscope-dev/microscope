//! Launching into the client's terminal with `runInTerminal`, as VS Code's
//! `"console": "integratedTerminal"` and `"externalTerminal"` ask.

use serde_json::{Value, json};

use crate::dap::{Configuration, Dap, Profile, fixture, line_of, source};
use crate::support::{self, ScratchDir};

#[test]
fn a_program_launched_in_a_terminal_is_debugged_with_its_streams_in_the_terminal() {
    let directory = ScratchDir::new("terminal-launch");
    let path = source("c/process-environment.c");
    let line = line_of(&path, "if (fgets(line");
    let mut dap = Dap::start("integrated terminal");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("process-environment"),
        json!({
            "console": "integratedTerminal",
            "args": ["one", "two words"],
            "env": {"MICROSCOPE_FIXTURE_VALUE": "set", "MICROSCOPE_FIXTURE_REMOVED": null},
            "cwd": directory.path(),
        }),
        &Configuration {
            sources: vec![(path, vec![line])],
            ..Configuration::default()
        },
    );
    let request = dap
        .messages_since(started.mark)
        .into_iter()
        .find(|message| message["type"] == "request")
        .expect("a runInTerminal request");
    assert_eq!(request["command"], "runInTerminal");
    assert_eq!(request["arguments"]["kind"], "integrated");
    assert_eq!(request["arguments"]["title"], "process-environment");
    assert_eq!(
        request["arguments"]["cwd"],
        directory.path().display().to_string()
    );

    // The terminal's process is the program, from its first instruction.
    let process = dap.event(started.mark, "process", |_| true);
    assert_eq!(process["startMethod"], "launch");
    let stop = dap.stopped(started.mark);
    assert_eq!(stop.reason, "breakpoint");
    assert_eq!(
        process["systemProcessId"],
        dap.process_id().expect("a process")
    );
    let frames = dap.inspect_as(Profile::VsCode, &stop);
    assert_eq!(frames[0]["name"], "main");
    assert_eq!(dap.terminal_line(), "argument 1: one");
    assert_eq!(dap.terminal_line(), "argument 2: two words");
    assert_eq!(dap.terminal_line(), "value: set");
    assert_eq!(dap.terminal_line(), "removed: absent");
    assert_eq!(
        dap.terminal_line(),
        format!("directory: {}", directory.path().display())
    );

    let resumed = dap.send("continue", json!({"threadId": stop.thread}));
    dap.success(resumed);
    assert_eq!(
        dap.event(resumed.mark, "exited", |_| true),
        json!({"exitCode": 3})
    );
    assert_eq!(dap.terminal_line(), "input: (eof)");

    // Nothing the program wrote reached the debug console.
    assert!(dap.output_text(started.mark, "stdout").is_empty());
    assert!(dap.output_text(started.mark, "stderr").is_empty());
    dap.event(resumed.mark, "terminated", |_| true);
    dap.finish();
}

#[test]
fn a_program_in_an_external_terminal_stops_on_entry_restarts_and_ends_with_the_session() {
    // The terminal's program is unrandomized too, though the client's
    // terminal starts it.
    support::randomize_addresses();
    let mut dap = Dap::start("external terminal");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("spin"),
        json!({"console": "externalTerminal", "stopOnEntry": true}),
        &Configuration::default(),
    );
    let stop = dap.stopped(started.mark);
    assert_eq!(stop.reason, "entry");
    let frames = dap.inspect_as(Profile::VsCode, &stop);
    assert!(
        frames[0]["name"]
            .as_str()
            .is_some_and(|name| name.contains("ld-linux")),
        "{frames:?}"
    );
    let entry = frames[0]["instructionPointerReference"].clone();
    let first = dap.process_id().expect("a process");

    // A restart runs the program in a new terminal.
    let restarted = dap.send("restart", Value::Null);
    dap.success(restarted);
    let requests = dap
        .messages_since(started.mark)
        .into_iter()
        .filter(|message| message["type"] == "request")
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| request["arguments"]["kind"] == "external")
    );

    let stop = dap.stopped(restarted.mark);
    assert_eq!(stop.reason, "entry");
    assert_ne!(dap.process_id(), Some(first));
    assert_eq!(
        dap.inspect_as(Profile::VsCode, &stop)[0]["instructionPointerReference"],
        entry
    );
    let resumed = dap.send("continue", json!({"threadId": stop.thread}));
    dap.success(resumed);

    // Disconnecting kills the program the terminal runs.
    dap.finish();
}

#[test]
fn a_terminal_launch_fails_when_the_client_cannot_run_one() {
    // Helix does not support runInTerminal.
    let mut dap = Dap::start("terminal unsupported");
    dap.initialize(Profile::Helix);
    let launch = dap.send(
        "launch",
        json!({"program": fixture("basic"), "console": "integratedTerminal"}),
    );
    dap.request("configurationDone", Value::Null);
    assert_eq!(
        dap.failure(launch),
        "this client cannot run programs in a terminal; set \"console\" to \"internalConsole\""
    );
    dap.event(launch.mark, "terminated", |_| true);
    dap.finish();

    // VS Code may fail to start the terminal.
    let mut dap = Dap::start("terminal refused");
    dap.refuse_terminals("no terminal is available");
    dap.initialize(Profile::VsCode);
    let launch = dap.send(
        "launch",
        json!({"program": fixture("basic"), "console": "integratedTerminal"}),
    );
    dap.request("configurationDone", Value::Null);
    let message = dap.failure(launch);
    assert!(
        message.ends_with(
            "basic: the client could not run it in a terminal: no terminal is available"
        ),
        "{message}"
    );
    dap.event(launch.mark, "terminated", |_| true);
    dap.finish();
}

/// A program that executes another is followed into it whether it runs in
/// the adapter or in the client's terminal: the terminal runs the program
/// the user named, not the one it executes, and the breakpoint only the
/// executed program has stops it there.
#[test]
fn a_program_that_executes_another_is_followed_into_it() {
    for console in ["internalConsole", "integratedTerminal"] {
        let mut dap = Dap::start(format!("exec, {console}"));
        let started = dap.launch(
            Profile::VsCode,
            &fixture("exec-other"),
            json!({"console": console}),
            &Configuration {
                functions: vec!["target_work".to_owned()],
                ..Configuration::default()
            },
        );
        if console == "integratedTerminal" {
            let request = dap
                .messages_since(started.mark)
                .into_iter()
                .find(|message| message["type"] == "request")
                .expect("a runInTerminal request");
            let args = request["arguments"]["args"].as_array().expect("args");
            assert!(
                args.last()
                    .and_then(Value::as_str)
                    .is_some_and(|program| program.ends_with("/exec-other")),
                "{request}"
            );
        }

        let note = dap.output_containing(started.mark, "console", " executed ");
        assert!(note.contains("exec-target"), "{note}");
        let stop = dap.stopped(started.mark);
        let frames = dap.inspect_as(Profile::VsCode, &stop);
        assert_eq!(frames[0]["name"], "target_work", "{console}");
        assert!(
            frames[0]["source"]["path"]
                .as_str()
                .is_some_and(|path| path.ends_with("exec-target.c")),
            "{}",
            frames[0]
        );

        let resumed = dap.send("continue", json!({"threadId": stop.thread}));
        dap.success(resumed);
        assert_eq!(dap.event(resumed.mark, "exited", |_| true)["exitCode"], 42);
        dap.finish();
    }
}
