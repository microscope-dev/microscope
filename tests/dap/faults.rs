//! Ending programs in every way a client or the system can: terminate
//! requests, pauses before the program runs, and programs killed from
//! outside.

use serde_json::{Value, json};

use crate::dap::{Configuration, Dap, Profile, fixture};

fn pid(process: u32) -> nix::unistd::Pid {
    nix::unistd::Pid::from_raw(i32::try_from(process).expect("pid fits i32"))
}

#[test]
fn terminate_asks_the_program_to_end_whether_running_or_stopped() {
    // A program that handles SIGTERM ends as it chooses, once its handler
    // is in place.
    let mut dap = Dap::start("terminate running");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("terminate"),
        json!({}),
        &Configuration {
            functions: vec!["terminate_tick".to_owned()],
            ..Configuration::default()
        },
    );
    let stop = dap.stopped(started.mark);
    dap.request("setFunctionBreakpoints", json!({"breakpoints": []}));
    dap.request("continue", json!({"threadId": stop.thread}));
    let terminated = dap.send("terminate", Value::Null);
    dap.success(terminated);
    assert_eq!(
        dap.event(terminated.mark, "exited", |_| true),
        json!({"exitCode": 7})
    );
    dap.event(terminated.mark, "terminated", |_| true);
    dap.finish();

    // A stopped program is resumed to receive it, which the client hears.
    let mut dap = Dap::start("terminate stopped");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("terminate"),
        json!({}),
        &Configuration {
            functions: vec!["terminate_tick".to_owned()],
            ..Configuration::default()
        },
    );
    let stop = dap.stopped(started.mark);
    let terminated = dap.send("terminate", Value::Null);
    dap.success(terminated);
    dap.event(terminated.mark, "continued", |body| {
        body["threadId"] == stop.thread
    });
    assert_eq!(
        dap.event(terminated.mark, "exited", |_| true),
        json!({"exitCode": 7})
    );
    dap.finish();

    // One that does not handle it dies of it.
    let mut dap = Dap::start("terminate unhandled");
    let started = dap.launch(
        Profile::Neovim,
        &fixture("spin"),
        json!({}),
        &Configuration::default(),
    );
    dap.event(started.mark, "process", |_| true);
    let terminated = dap.send("terminate", Value::Null);
    dap.success(terminated);
    assert_eq!(
        dap.event(terminated.mark, "exited", |_| true),
        json!({"exitCode": 128 + 15})
    );
    dap.finish();
}

#[test]
fn terminate_lets_a_program_end_by_raising_the_signal_again() {
    // A program that cleans up after the debugger's SIGTERM and raises it
    // again, as Go's runtime does, dies of the second without stopping.
    let mut dap = Dap::start("terminate reraised");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("terminate"),
        json!({"args": ["reraise"]}),
        &Configuration {
            functions: vec!["terminate_tick".to_owned()],
            ..Configuration::default()
        },
    );
    dap.stopped(started.mark);
    dap.request("setFunctionBreakpoints", json!({"breakpoints": []}));
    let terminated = dap.send("terminate", Value::Null);
    dap.success(terminated);
    assert_eq!(
        dap.next_event(terminated.mark, &["stopped", "exited"]),
        ("exited".to_owned(), json!({"exitCode": 128 + 15}))
    );
    dap.finish();

    // Once the user has stopped in the handler, the program's own signal is
    // its policy's again.
    let mut dap = Dap::start("terminate reraised after a stop");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("terminate"),
        json!({"args": ["reraise"]}),
        &Configuration {
            functions: vec!["terminate_tick".to_owned()],
            ..Configuration::default()
        },
    );
    dap.stopped(started.mark);
    dap.request(
        "setFunctionBreakpoints",
        json!({"breakpoints": [{"name": "request_termination"}]}),
    );
    let terminated = dap.send("terminate", Value::Null);
    dap.success(terminated);
    let handler = dap.stopped(terminated.mark);
    assert_eq!(handler.reason, "function breakpoint");
    let resumed = dap.send("continue", json!({"threadId": handler.thread}));
    dap.success(resumed);
    let reraised = dap.event(resumed.mark, "stopped", |_| true);
    assert_eq!(
        (&reraised["reason"], &reraised["text"]),
        (&json!("exception"), &json!("SIGTERM"))
    );
    dap.finish();
}

#[test]
fn pausing_before_the_program_runs_is_refused_and_right_after_it_starts_stops_it() {
    let mut dap = Dap::start("pause while launching");
    dap.initialize(Profile::Neovim);
    let launch = dap.send("launch", json!({"program": fixture("spin")}));

    // Nothing runs until configuration is done.
    assert_eq!(
        dap.request_error("pause", json!({"threadId": 1})),
        "the program is not running"
    );

    // A pause sent with configurationDone stops the program it starts.
    let done = dap.send("configurationDone", Value::Null);
    let paused = dap.send("pause", json!({"threadId": 1}));
    dap.success(done);
    dap.success(launch);
    dap.success(paused);

    let stop = dap.stopped(launch.mark);
    assert_eq!(stop.reason, "pause");
    dap.finish();
}

#[test]
fn a_program_killed_from_outside_ends_the_session_whether_running_or_stopped() {
    for stopped in [false, true] {
        let mut dap = Dap::start(format!("killed outside, stopped {stopped}"));
        let started = dap.launch(
            Profile::VsCode,
            &fixture("terminate"),
            json!({}),
            &Configuration {
                functions: if stopped {
                    vec!["terminate_tick".to_owned()]
                } else {
                    Vec::new()
                },
                ..Configuration::default()
            },
        );
        if stopped {
            dap.stopped(started.mark);
        } else {
            dap.event(started.mark, "process", |_| true);
        }

        let mark = dap.mark();
        nix::sys::signal::kill(
            pid(dap.process_id().expect("process")),
            nix::sys::signal::Signal::SIGKILL,
        )
        .expect("kill the program");
        assert_eq!(
            dap.event(mark, "exited", |_| true),
            json!({"exitCode": 128 + 9})
        );
        dap.event(mark, "terminated", |_| true);

        // Requests that need a program say there is none.
        assert_eq!(
            dap.request_error("stackTrace", json!({"threadId": 1})),
            "the program is not running"
        );
        dap.finish();
    }
}
