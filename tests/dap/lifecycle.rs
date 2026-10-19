//! Starting and ending sessions: launch, attach, core dumps, configuration
//! errors, and every way a session ends.

use serde_json::{Value, json};

use crate::dap::{Configuration, Dap, Profile, fixture, line_of, source};
use crate::support::{ExternalProcess, ScratchDir};

#[test]
fn every_client_profile_runs_a_program_to_its_breakpoints_and_exit() {
    let path = source("c/basic.c");
    let line = line_of(&path, "return microscope_value;");
    for profile in Profile::ALL {
        let mut dap = Dap::start(format!("{profile:?}"));
        let started = dap.launch(
            profile,
            &fixture("basic"),
            json!({}),
            &Configuration {
                sources: vec![(path.clone(), vec![line])],
                functions: vec!["main".to_owned()],
                ..Configuration::default()
            },
        );
        let source_id = started.source_breakpoints[0][0]["id"].clone();
        let function_id = started.function_breakpoints[0]["id"].clone();
        let mut breakpoints = [
            started.source_breakpoints[0][0].clone(),
            started.function_breakpoints[0].clone(),
        ];
        if profile == Profile::DeferredLaunch {
            // Configured before the program is loaded, the breakpoints wait
            // for it and then resolve.
            for breakpoint in &mut breakpoints {
                assert_eq!(breakpoint["verified"], false);
                assert_eq!(breakpoint["reason"], "pending");
                let id = breakpoint["id"].clone();
                *breakpoint = dap.event(started.mark, "breakpoint", |body| {
                    body["breakpoint"]["id"] == id
                })["breakpoint"]
                    .clone();
            }
        }

        assert_eq!(breakpoints[0]["verified"], true);
        assert_eq!(breakpoints[0]["line"], line);
        assert_eq!(breakpoints[1]["verified"], true);
        assert_eq!(breakpoints[1]["line"], line_of(&path, "int main(void)") + 1);

        let stop = dap.stopped(started.mark);
        assert_eq!(stop.reason, "function breakpoint", "{profile:?}");
        assert_eq!(stop.body["hitBreakpointIds"], json!([function_id]));
        let frames = dap.inspect_as(profile, &stop);
        assert_eq!(frames[0]["name"], "main");
        assert_eq!(frames[0]["source"]["path"], path.display().to_string());

        // The function is called twice, so its breakpoint stops twice.
        for _ in 0..2 {
            let resumed = dap.send("continue", json!({"threadId": stop.thread}));
            assert_eq!(dap.success(resumed), json!({"allThreadsContinued": true}));
            let hit = dap.stopped(resumed.mark);
            assert_eq!(hit.reason, "breakpoint");
            assert_eq!(hit.body["hitBreakpointIds"], json!([source_id]));
            let frames = dap.inspect_as(profile, &hit);
            assert_eq!(frames[0]["name"], "breakpoint_target");
            assert_eq!(frames[0]["line"], line);
            assert_eq!(frames[1]["name"], "main");
        }

        let resumed = dap.send("continue", json!({"threadId": stop.thread}));
        dap.success(resumed);
        assert_eq!(
            dap.event(resumed.mark, "exited", |_| true),
            json!({"exitCode": 0})
        );
        dap.event(resumed.mark, "terminated", |_| true);
        dap.finish();
    }
}

#[test]
fn launches_pass_arguments_environment_and_directory_and_report_output() {
    let directory = ScratchDir::new("dap-launch");
    let mut dap = Dap::start("launch options");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("process-environment"),
        json!({
            "args": ["one", "two words"],
            "env": {"MICROSCOPE_FIXTURE_VALUE": "set", "MICROSCOPE_FIXTURE_REMOVED": null},
            "cwd": directory.path(),
        }),
        &Configuration::default(),
    );
    assert_eq!(
        dap.event(started.mark, "exited", |_| true),
        json!({"exitCode": 3})
    );

    // The program's output is complete when it exits.
    assert_eq!(
        dap.output_text(started.mark, "stdout"),
        format!(
            "argument 1: one\nargument 2: two words\nvalue: set\nremoved: absent\ndirectory: {}\n",
            directory.path().display()
        )
    );

    // Its stdin is empty rather than the adapter's protocol stream.
    assert_eq!(dap.output_text(started.mark, "stderr"), "input: (eof)\n");
    let process = dap.event(started.mark, "process", |_| true);
    assert_eq!(process["startMethod"], "launch");
    assert_eq!(
        process["name"],
        fixture("process-environment").display().to_string()
    );
    dap.event(started.mark, "terminated", |_| true);
    dap.finish();
}

#[test]
fn stop_on_entry_stops_before_the_program_runs_any_code() {
    let mut dap = Dap::start("stop on entry");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("process-environment"),
        json!({"stopOnEntry": true}),
        &Configuration::default(),
    );
    let stop = dap.stopped(started.mark);
    assert_eq!(stop.reason, "entry");
    let frames = dap.inspect_as(Profile::VsCode, &stop);

    // The first instruction belongs to the dynamic loader.
    assert!(
        frames[0]["name"]
            .as_str()
            .is_some_and(|name| name.contains("ld-linux")),
        "{frames:?}"
    );
    assert!(dap.output_text(started.mark, "stdout").is_empty());
    let resumed = dap.send("continue", json!({"threadId": stop.thread}));
    dap.success(resumed);
    assert_eq!(
        dap.event(resumed.mark, "exited", |_| true),
        json!({"exitCode": 1})
    );
    dap.finish();
}

#[test]
fn invalid_configurations_are_shown_to_the_user_and_the_session_survives() {
    let missing = fixture("does-not-exist");
    for (command, arguments, expected) in [
        (
            "launch",
            json!({}),
            "invalid launch configuration: missing field `program`".to_owned(),
        ),
        (
            "launch",
            json!({"program": fixture("basic"), "env": {"PATH": 1}}),
            "invalid launch configuration at env.PATH: invalid type: integer `1`, expected a string".to_owned(),
        ),
        (
            "launch",
            json!({"program": fixture("basic"), "console": "pty"}),
            "invalid launch configuration at console: unknown variant `pty`, expected one of `internalConsole`, `integratedTerminal`, `externalTerminal`".to_owned(),
        ),
        (
            "launch",
            json!({"program": missing}),
            format!("failed to load {}: ", missing.display()),
        ),
        (
            "attach",
            json!({"program": fixture("basic")}),
            "invalid attach configuration: give `pid` or `coreFile`".to_owned(),
        ),
        (
            "attach",
            json!({"pid": "not a number"}),
            "invalid attach configuration at pid: expected a positive process id".to_owned(),
        ),
        (
            "attach",
            json!({"pid": 999_999_999}),
            "failed to attach to process 999999999: ".to_owned(),
        ),
        (
            "attach",
            json!({"coreFile": missing}),
            format!("failed to open the core dump {}: ", missing.display()),
        ),
    ] {
        let mut dap = Dap::start(format!("{command} {arguments}"));
        dap.initialize(Profile::VsCode);
        let sent = dap.send(command, arguments);
        let response = dap.response(sent);
        assert_eq!(response["success"], false);
        assert_eq!(response["body"]["error"]["showUser"], true);
        let message = response["message"].as_str().expect("message");
        assert!(message.starts_with(&expected), "{message}");

        // Requests still work after the failure.
        dap.request("threads", Value::Null);
        dap.finish();
    }
}

#[test]
fn requests_out_of_order_are_refused_without_ending_the_session() {
    let mut dap = Dap::start("ordering");
    assert_eq!(
        dap.request_error("threads", Value::Null),
        "the client must send initialize before threads"
    );
    dap.initialize(Profile::Neovim);
    assert_eq!(
        dap.request_error("initialize", json!({"adapterID": "microscope"})),
        "the session is already initialized"
    );
    assert_eq!(
        dap.request_error("stackTrace", json!({"threadId": 1})),
        "the program is not running"
    );
    assert_eq!(
        dap.request_error("frobnicate", json!({})),
        "the 'frobnicate' request is not supported"
    );

    // Every source has a path, so a client never needs its contents sent.
    assert_eq!(
        dap.request_error("source", json!({"sourceReference": 1})),
        "source contents are not available from the debugger; open the file locally"
    );

    // A thread to pause exists even before anything is launched.
    assert_eq!(
        dap.request("threads", Value::Null),
        json!({"threads": [{"id": 1, "name": "program"}]})
    );

    let launch = dap.send("launch", json!({"program": fixture("basic")}));
    dap.request("configurationDone", Value::Null);
    dap.success(launch);
    assert_eq!(
        dap.request_error("launch", json!({"program": fixture("basic")})),
        "the session already has a program; start a new session for another"
    );
    assert_eq!(
        dap.request_error("configurationDone", Value::Null),
        "configuration is already done"
    );
    dap.event(launch.mark, "terminated", |_| true);
    dap.finish();
}

#[test]
fn disconnecting_kills_a_launched_program_whether_running_or_stopped() {
    for stopped in [false, true] {
        let mut dap = Dap::start(format!("disconnect stopped={stopped}"));
        let started = dap.launch(
            Profile::Helix,
            &fixture("spin"),
            json!({"stopOnEntry": stopped}),
            &Configuration::default(),
        );
        if stopped {
            dap.stopped(started.mark);
        } else {
            dap.event(started.mark, "process", |_| true);
        }

        let sent = dap.send("disconnect", json!({}));
        dap.success(sent);

        // The program's end is reported before the disconnect response.
        let messages = dap.messages_since(sent.mark);
        let position = |predicate: &dyn Fn(&Value) -> bool| messages.iter().position(predicate);
        let exited = position(&|message| message["event"] == "exited").expect("exited");
        let terminated = position(&|message| message["event"] == "terminated").expect("terminated");
        let response = position(&|message| message["type"] == "response").expect("response");
        assert!(exited < terminated && terminated < response, "{messages:?}");
        assert_eq!(messages[exited]["body"], json!({"exitCode": 128 + 9}));

        // Finishing checks that the program is gone.
        dap.close_stdin();
        dap.finish();
    }
}

#[test]
fn ending_input_or_signaling_the_adapter_ends_the_session_and_the_program() {
    for signal in [
        None,
        Some(nix::sys::signal::Signal::SIGTERM),
        Some(nix::sys::signal::Signal::SIGINT),
    ] {
        let mut dap = Dap::start(format!("end by {signal:?}"));
        let started = dap.launch(
            Profile::VsCode,
            &fixture("spin"),
            json!({}),
            &Configuration::default(),
        );
        dap.event(started.mark, "process", |_| true);
        match signal {
            None => dap.close_stdin(),
            Some(signal) => dap.signal(signal),
        }

        dap.wait_for_exit();
        dap.close_stdin();
        dap.finish();
    }
}

#[test]
fn attaching_continues_the_process_and_detaching_leaves_it_running() {
    for stop_on_entry in [false, true] {
        let mut process = ExternalProcess::spawn(&fixture("attach"));
        let pid = process.process_id().get();
        let mut dap = Dap::start(format!("attach stopOnEntry={stop_on_entry}"));
        let started = dap.begin(
            Profile::VsCode,
            (
                "attach",
                json!({"pid": pid.to_string(), "stopOnEntry": stop_on_entry}),
            ),
            &Configuration {
                functions: vec!["attach_breakpoint".to_owned()],
                ..Configuration::default()
            },
        );
        let event = dap.event(started.mark, "process", |_| true);
        assert_eq!(event["startMethod"], "attach");
        assert_eq!(event["systemProcessId"], pid);

        // Reading the process's debug information shows progress.
        let progress = dap.event(started.mark, "progressStart", |_| true);
        assert_eq!(progress["title"], format!("Attaching to process {pid}"));
        dap.event(started.mark, "progressEnd", |_| true);
        if stop_on_entry {
            let stop = dap.stopped(started.mark);
            assert_eq!(stop.reason, "entry");
            dap.inspect_as(Profile::VsCode, &stop);
            let resumed = dap.send("continue", json!({"threadId": stop.thread}));
            dap.success(resumed);
        }

        let mark = dap.mark();
        process.release();
        let stop = dap.stopped(mark);
        assert_eq!(stop.reason, "function breakpoint");
        let frames = dap.inspect_as(Profile::VsCode, &stop);
        assert_eq!(frames[0]["name"], "attach_breakpoint");

        // Detaching leaves the process running; it then finishes alone.
        dap.finish();
        assert_eq!(process.wait().code(), Some(23));
    }
}

#[test]
fn attaching_with_terminate_debuggee_kills_the_process() {
    let process = ExternalProcess::spawn(&fixture("attach"));
    let mut dap = Dap::start("attach and terminate");
    let started = dap.begin(
        Profile::Neovim,
        ("attach", json!({"pid": process.process_id().get()})),
        &Configuration::default(),
    );
    dap.event(started.mark, "process", |_| true);
    let sent = dap.send("disconnect", json!({"terminateDebuggee": true}));
    dap.success(sent);
    dap.event(sent.mark, "exited", |_| true);
    dap.close_stdin();
    dap.wait_for_exit();
    dap.abandon();
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&process.wait()),
        Some(9)
    );
}

#[test]
fn core_dumps_open_stopped_at_their_signal_and_refuse_to_run() {
    let mut dap = Dap::start("core");
    let started = dap.begin(
        Profile::VsCode,
        (
            "attach",
            json!({"coreFile": fixture("crash-gcc-o0-segv.core"), "program": fixture("crash-gcc-o0")}),
        ),
        &Configuration::default(),
    );
    let stop = dap.stopped(started.mark);
    assert_eq!(stop.reason, "exception");
    assert_eq!(stop.body["text"], "SIGSEGV");

    // A core dump has no program to restart, so a client opens it again.
    assert_eq!(
        dap.event(started.mark, "capabilities", |_| true),
        json!({"capabilities": {"supportsRestartRequest": false}})
    );
    let info = dap.request("exceptionInfo", json!({"threadId": stop.thread}));
    assert_eq!(info["exceptionId"], "SIGSEGV");
    assert_eq!(info["breakMode"], "always");
    let frames = dap.inspect_as(Profile::VsCode, &stop);
    assert!(frames.iter().any(|frame| {
        frame["source"]["path"]
            .as_str()
            .is_some_and(|path| path.ends_with("crash/main.c"))
    }));
    let message = dap.request_error("continue", json!({"threadId": stop.thread}));
    assert!(message.contains("core dump"), "{message}");
    dap.finish();
}

#[test]
fn restarting_relaunches_the_program_with_new_arguments_in_the_same_session() {
    let mut dap = Dap::start("restart");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("process-environment"),
        json!({"args": ["one"], "stopOnEntry": true}),
        &Configuration::default(),
    );
    let entry = dap.stopped(started.mark);
    let first = dap.process_id().expect("first process");
    let restart = dap.send(
        "restart",
        json!({"arguments": {"program": fixture("process-environment"), "args": ["a", "b", "c"]}}),
    );
    dap.success(restart);

    // The old process ends, but the session goes on.
    assert_eq!(
        dap.event(restart.mark, "exited", |_| true),
        json!({"exitCode": 128 + 9})
    );
    let exited = dap.event(restart.mark, "thread", |body| body["reason"] == "exited");
    assert_eq!(exited["threadId"], entry.thread);
    let process = dap.event(restart.mark, "process", |_| true);
    assert_ne!(process["systemProcessId"], first);
    assert_eq!(
        dap.event(restart.mark, "exited", |_| true),
        json!({"exitCode": 4})
    );
    dap.event(restart.mark, "terminated", |_| true);
    assert!(
        dap.output_text(restart.mark, "stdout")
            .starts_with("argument 1: a\nargument 2: b\nargument 3: c\n")
    );

    let terminations = dap
        .messages_since(restart.mark)
        .iter()
        .filter(|message| message["event"] == "terminated")
        .count();
    assert_eq!(
        terminations, 1,
        "only the restarted program's end ends the session"
    );
    dap.finish();
}

#[test]
fn restarts_keep_the_program_and_attached_processes_are_attached_again() {
    let mut dap = Dap::start("restart refused");
    let started = dap.launch(
        Profile::Neovim,
        &fixture("spin"),
        json!({}),
        &Configuration::default(),
    );
    dap.event(started.mark, "process", |_| true);
    assert_eq!(
        dap.request_error(
            "restart",
            json!({"arguments": {"program": fixture("basic")}})
        ),
        "a restart cannot change the program; start a new session for another"
    );
    dap.finish();

    // An attached process is restarted by attaching to it again, which a
    // client does itself; the disconnect that begins it detaches, even
    // when it asks to terminate, so the process is there to attach to.
    let mut process = ExternalProcess::spawn(&fixture("attach"));
    let mut dap = Dap::start("restart attached");
    let started = dap.begin(
        Profile::VsCode,
        ("attach", json!({"pid": process.process_id().get()})),
        &Configuration::default(),
    );
    dap.event(started.mark, "process", |_| true);
    assert_eq!(
        dap.event(started.mark, "capabilities", |_| true),
        json!({"capabilities": {"supportsRestartRequest": false}})
    );
    assert_eq!(
        dap.request_error("restart", Value::Null),
        "only a launched program can be restarted; start a new session instead"
    );
    dap.finish_with(json!({"restart": true, "terminateDebuggee": true}));

    let mut dap = Dap::start("attach again");
    let started = dap.begin(
        Profile::VsCode,
        ("attach", json!({"pid": process.process_id().get()})),
        &Configuration {
            functions: vec!["attach_breakpoint".to_owned()],
            ..Configuration::default()
        },
    );
    dap.event(started.mark, "process", |_| true);
    let mark = dap.mark();
    process.release();
    assert_eq!(dap.stopped(mark).reason, "function breakpoint");
    dap.finish();
    assert_eq!(process.wait().code(), Some(23));
}

#[test]
fn core_dumps_explain_their_signal_their_missing_modules_and_mismatched_files() {
    // An abort, as a fatal signal the program sent itself.
    let mut dap = Dap::start("abort core");
    let started = dap.begin(
        Profile::Neovim,
        (
            "attach",
            json!({"coreFile": fixture("crash-gcc-o0-abort.core"), "program": fixture("crash-gcc-o0")}),
        ),
        &Configuration::default(),
    );
    let stop = dap.stopped(started.mark);
    assert_eq!(
        (stop.reason.as_str(), &stop.body["text"]),
        ("exception", &json!("SIGABRT"))
    );
    dap.finish();

    // A library the core records but this machine lacks is reported, and
    // the rest of the dump is still debugged.
    let core = fixture("core-missing-library/crash.core");
    let mut dap = Dap::start("core missing a library");
    let started = dap.begin(
        Profile::VsCode,
        ("attach", json!({"coreFile": core})),
        &Configuration::default(),
    );
    let warning = dap.output_containing(started.mark, "important", "libcrash.so is missing");
    assert!(
        warning.contains("its frames and unsaved memory are unavailable"),
        "{warning}"
    );
    let stop = dap.stopped(started.mark);
    assert_eq!(stop.body["text"], "SIGSEGV");
    dap.inspect_as(Profile::VsCode, &stop);
    dap.finish();

    // A rebuilt executable is refused unless mismatches are allowed.
    for allowed in [false, true] {
        let mut dap = Dap::start(format!("mismatched core, allowed {allowed}"));
        dap.initialize(Profile::VsCode);
        let attach = dap.send(
            "attach",
            json!({
                "coreFile": fixture("crash-gcc-o0-segv.core"),
                "program": fixture("crash-gcc-o0-rebuilt"),
                "allowModuleMismatch": allowed,
            }),
        );
        dap.request("configurationDone", Value::Null);
        if allowed {
            dap.success(attach);
            assert_eq!(dap.stopped(attach.mark).body["text"], "SIGSEGV");
        } else {
            let message = dap.failure(attach);
            assert!(
                message.contains("the build-id note differs; allow module mismatches"),
                "{message}"
            );
        }

        dap.finish();
    }
}
