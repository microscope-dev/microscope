//! Running, stepping, pausing, and the stops signals and exits cause.

use serde_json::{Value, json};

use crate::dap::{Configuration, Dap, Profile, Stopped, fixture, line_of, source};

/// Launches `program` stopped at a breakpoint on the first line holding
/// `marker` in `path`.
fn stopped_at(dap: &mut Dap, program: &str, path: &str, marker: &str) -> Stopped {
    let path = source(path);
    let line = line_of(&path, marker);
    let started = dap.launch(
        Profile::VsCode,
        &fixture(program),
        json!({}),
        &Configuration {
            sources: vec![(path, vec![line])],
            ..Configuration::default()
        },
    );
    let stop = dap.stopped(started.mark);
    assert_eq!(stop.reason, "breakpoint");

    stop
}

/// The innermost frame of a thread: its function name and line.
fn top(dap: &mut Dap, thread: i64) -> (String, Value, Value) {
    let trace = dap.request("stackTrace", json!({"threadId": thread, "levels": 1}));
    let frame = &trace["stackFrames"][0];
    (
        frame["name"].as_str().expect("name").to_owned(),
        frame["line"].clone(),
        frame["instructionPointerReference"].clone(),
    )
}

#[test]
fn another_threads_breakpoint_ends_a_step_where_it_hit() {
    let mut dap = Dap::start("interrupted step");
    let path = source("c/thread-steps.c");
    let line = line_of(&path, "joins the gated worker");
    let started = dap.launch(
        Profile::Neovim,
        &fixture("thread-steps-gcc-o0"),
        json!({}),
        &Configuration {
            sources: vec![(path, vec![line])],
            functions: vec!["worker_reached".to_owned()],
            ..Configuration::default()
        },
    );
    let worker_breakpoint = started.function_breakpoints[0]["id"].clone();
    let stop = dap.stopped(started.mark);
    let next = dap.send("next", json!({"threadId": stop.thread}));
    dap.success(next);
    let interrupted = dap.stopped(next.mark);
    assert_eq!(interrupted.reason, "function breakpoint");
    assert_ne!(interrupted.thread, stop.thread);
    assert_eq!(
        interrupted.body["hitBreakpointIds"],
        json!([worker_breakpoint])
    );
    let (name, ..) = top(&mut dap, interrupted.thread);
    assert_eq!(name, "worker_reached");

    // Threads are named as they named themselves by the stop, and say
    // what stopped them.
    let threads = dap.request("threads", Value::Null);
    let worker = threads["threads"]
        .as_array()
        .expect("threads")
        .iter()
        .find(|thread| thread["id"] == interrupted.thread)
        .expect("the worker is listed")
        .clone();
    assert_eq!(
        worker["name"],
        format!(
            "gated-worker ({}) — at breakpoint {worker_breakpoint}",
            interrupted.thread
        )
    );

    // The abandoned step leaves nothing behind: the program now runs out.
    let resumed = dap.send("continue", json!({"threadId": interrupted.thread}));
    dap.success(resumed);
    assert_eq!(
        dap.event(resumed.mark, "exited", |_| true),
        json!({"exitCode": 0})
    );
    dap.finish();
}

/// One `stopped` event names the stop's thread, and every other thread
/// the stop found at a breakpoint is named as stopped by it.
#[test]
fn every_co_hit_thread_is_named_by_its_breakpoint() {
    let mut dap = Dap::start("co-hits");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("hit-count-threads"),
        json!({}),
        &Configuration {
            functions: vec!["contended".to_owned()],
            ..Configuration::default()
        },
    );
    let mut mark = started.mark;
    let mut co_hits = 0;

    for round in 0..10 {
        let stop = dap.stopped(mark);
        assert_eq!(stop.body["allThreadsStopped"], true, "{stop:?}");
        let threads = dap.request("threads", Value::Null)["threads"]
            .as_array()
            .expect("threads")
            .clone();

        // The adapter sends every event of a stop before it answers the
        // next request.
        let events = dap
            .messages_since(mark)
            .into_iter()
            .filter(|message| message["type"] == "event" && message["event"] == "stopped")
            .count();
        assert_eq!(events, 1, "{threads:?}");
        let at_breakpoint = threads
            .iter()
            .filter(|thread| {
                thread["name"]
                    .as_str()
                    .is_some_and(|name| name.ends_with(" — at breakpoint 1"))
            })
            .map(|thread| thread["id"].as_i64().expect("an id"))
            .collect::<Vec<_>>();
        assert!(at_breakpoint.contains(&stop.thread), "{threads:?}");
        co_hits += at_breakpoint.len() - 1;

        // The program stays at the last stop, which nothing would wait
        // for after it.
        if round == 9 {
            break;
        }

        let resumed = dap.send("continue", json!({"threadId": stop.thread}));
        dap.success(resumed);
        mark = resumed.mark;
    }

    // The workers call `contended` together, so many of their hits come
    // at the same stop.
    assert!(co_hits > 0);
    dap.finish();
}

#[test]
fn step_in_out_and_by_instruction() {
    let path = source("c/variables.c");
    let mut dap = Dap::start("steps");
    let stop = stopped_at(
        &mut dap,
        "variables-gcc-o0",
        "c/variables.c",
        "if (parameter_target(4)",
    );
    let thread = stop.thread;

    // The line's breakpoint covers several of its addresses, which steps
    // would otherwise reach and report as breakpoint stops.
    dap.request(
        "setBreakpoints",
        json!({"source": {"path": path}, "breakpoints": []}),
    );
    let advance = |dap: &mut Dap, command: &str, arguments: Value| {
        let sent = dap.send(command, arguments);
        dap.success(sent);

        // Clients assume only the stepping thread runs unless told.
        assert_eq!(
            dap.event(sent.mark, "continued", |_| true),
            json!({"threadId": thread, "allThreadsContinued": true})
        );
        let stopped = dap.stopped(sent.mark);
        assert_eq!(stopped.reason, "step", "{command}");
        top(dap, thread)
    };

    let (name, line, _) = advance(&mut dap, "stepIn", json!({"threadId": thread}));
    assert_eq!(
        (name.as_str(), line),
        (
            "parameter_target",
            json!(line_of(&path, "int local = parameter + 1;"))
        )
    );
    let (name, ..) = advance(&mut dap, "stepOut", json!({"threadId": thread}));
    assert_eq!(name, "main");

    // An instruction step moves by one instruction without leaving main.
    let (_, _, before) = top(&mut dap, thread);
    let (name, _, after) = advance(
        &mut dap,
        "next",
        json!({"threadId": thread, "granularity": "instruction"}),
    );
    assert_eq!(name, "main");
    assert_ne!(before, after);
    let (_, _, again) = advance(
        &mut dap,
        "stepIn",
        json!({"threadId": thread, "granularity": "instruction"}),
    );
    assert_ne!(after, again);
    dap.finish();
}

#[test]
fn pausing_stops_a_running_program_and_is_harmless_when_stopped() {
    let mut dap = Dap::start("pause");
    let started = dap.launch(
        Profile::Helix,
        &fixture("spin"),
        json!({}),
        &Configuration {
            functions: vec!["main".to_owned()],
            ..Configuration::default()
        },
    );

    // Run from main, which then loops forever, so the pause cannot land in
    // the loader or the C library's startup.
    let entered = dap.stopped(started.mark);
    dap.request("setFunctionBreakpoints", json!({"breakpoints": []}));
    let running = dap.send("continue", json!({"threadId": entered.thread}));
    dap.success(running);

    // Requests that need a stop say so while the program runs.
    assert_eq!(
        dap.request_error("stackTrace", json!({"threadId": 1})),
        "the program is running; this request needs it stopped"
    );
    assert!(
        !dap.request("threads", Value::Null)["threads"]
            .as_array()
            .expect("threads")
            .is_empty()
    );
    let paused = dap.send("pause", json!({"threadId": 1}));
    dap.success(paused);
    let stop = dap.stopped(paused.mark);
    assert_eq!(stop.reason, "pause");
    let (name, ..) = top(&mut dap, stop.thread);
    assert_eq!(name, "main");

    // Pausing again changes nothing and reports no second stop.
    dap.request("pause", json!({"threadId": stop.thread}));
    let resumed = dap.send("continue", json!({"threadId": stop.thread}));
    dap.success(resumed);
    let paused = dap.send("pause", json!({"threadId": stop.thread}));
    dap.success(paused);
    assert_eq!(dap.stopped(resumed.mark).reason, "pause");
    dap.finish();
}

/// Runs the signal fixture with exception settings and returns the
/// signals that stopped it, its exit code, and the console's text.
fn signal_stops(exceptions: Value) -> (Vec<String>, Value, String) {
    let mut dap = Dap::start(format!("signals {exceptions}"));
    let started = dap.launch(
        Profile::Neovim,
        &fixture("signal-policy"),
        json!({}),
        &Configuration {
            exceptions: Some(exceptions),
            ..Configuration::default()
        },
    );
    let mut mark = started.mark;
    let mut stops = Vec::new();

    let exit = loop {
        let (kind, body) = dap.next_event(mark, &["stopped", "exited"]);
        if kind == "exited" {
            break body["exitCode"].clone();
        }

        assert_eq!(body["reason"], "exception");
        let thread = body["threadId"].clone();
        let info = dap.request("exceptionInfo", json!({"threadId": thread}));
        assert_eq!(info["exceptionId"], body["text"]);
        assert_eq!(info["breakMode"], "always");
        stops.push(info["exceptionId"].as_str().expect("id").to_owned());
        let resumed = dap.send("continue", json!({"threadId": thread}));
        dap.success(resumed);
        mark = resumed.mark;
    };

    let console = dap.output_text(started.mark, "console");
    dap.finish();
    (stops, exit, console)
}

#[test]
fn signals_stop_as_exceptions_that_the_filters_choose() {
    // Every signal the program raises is handled; the exit status has a
    // bit for each handler that ran, so passing every signal makes 63.
    // The fixture's SIGRTMIN + 1: glibc reserves two real-time signals.
    let realtime = "SIG35".to_owned();
    let defaults = json!({"filters": ["fatal", "interrupt", "other"]});
    let (stops, exit, console) = signal_stops(defaults);
    assert_eq!(stops, ["SIGUSR1".to_owned(), realtime.clone()]);
    assert_eq!(exit, 63);
    assert!(
        !console.contains("SIGALRM"),
        "routine signals stay quiet: {console}"
    );

    let (stops, exit, console) = signal_stops(json!({"filters": []}));
    assert!(stops.is_empty());
    assert_eq!(exit, 63);

    // Signals that stop by default are still reported when they do not.
    assert!(console.contains("received SIGUSR1"), "{console}");
    assert!(
        console.contains(&format!("received {realtime}")),
        "{console}"
    );

    let (stops, exit, _) = signal_stops(json!({"filters": ["routine", "other"]}));
    assert_eq!(
        stops,
        [
            "SIGUSR1",
            "SIGALRM",
            "SIGURG",
            "SIGCHLD",
            "SIGWINCH",
            realtime.as_str()
        ]
    );
    assert_eq!(exit, 63);

    let (stops, exit, _) = signal_stops(json!({
        "filters": [],
        "filterOptions": [{"filterId": "other", "condition": "SIGWINCH, SIGUSR1"}],
    }));
    assert_eq!(stops, ["SIGUSR1", "SIGWINCH"]);
    assert_eq!(exit, 63);
}

#[test]
fn fatal_signals_explain_themselves_and_exits_report_the_signal() {
    let mut dap = Dap::start("fatal signal");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("fatal-signal"),
        json!({}),
        &Configuration::default(),
    );
    let stop = dap.stopped(started.mark);
    assert_eq!(
        (stop.reason.as_str(), &stop.body["text"]),
        ("exception", &json!("SIGSEGV"))
    );
    let info = dap.request("exceptionInfo", json!({"threadId": stop.thread}));

    // The description names the fault's cause and address.
    assert_eq!(info["description"], "SIGSEGV (SEGV_MAPERR) at 0x1");
    assert_eq!(top(&mut dap, stop.thread).0, "fault");
    assert!(
        dap.request_error("exceptionInfo", json!({"threadId": stop.thread + 1}))
            .contains("did not cause the stop")
    );
    let resumed = dap.send("continue", json!({"threadId": stop.thread}));
    dap.success(resumed);

    // Death by a signal is an exit code no client mistakes for a status.
    assert_eq!(
        dap.event(resumed.mark, "exited", |_| true),
        json!({"exitCode": 128 + 11})
    );
    assert!(
        dap.output_text(resumed.mark, "important")
            .contains("the program was terminated by SIGSEGV")
    );
    dap.finish();
}

#[test]
fn single_thread_requests_run_only_the_thread_they_name() {
    let mut dap = Dap::start("single thread");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("hot-calls"),
        json!({}),
        &Configuration {
            functions: vec!["hot_function".to_owned()],
            ..Configuration::default()
        },
    );
    assert_eq!(
        started.capabilities["supportsSingleThreadExecutionRequests"],
        true
    );
    let first = dap.stopped(started.mark);

    // Only the named thread runs, so only it can stop again, though every
    // thread calls the function.
    for _ in 0..5 {
        let resumed = dap.send(
            "continue",
            json!({"threadId": first.thread, "singleThread": true}),
        );
        assert_eq!(dap.success(resumed), json!({"allThreadsContinued": false}));
        assert_eq!(dap.stopped(resumed.mark).thread, first.thread);
    }

    let stepped = dap.send(
        "next",
        json!({"threadId": first.thread, "singleThread": true}),
    );
    dap.success(stepped);
    let stop = dap.stopped(stepped.mark);
    assert_eq!((stop.reason.as_str(), stop.thread), ("step", first.thread));
    dap.request("setFunctionBreakpoints", json!({"breakpoints": []}));
    dap.request(
        "setExpression",
        json!({"expression": "hot_stop", "value": "1"}),
    );
    let resumed = dap.send("continue", json!({"threadId": stop.thread}));
    dap.success(resumed);
    assert_eq!(
        dap.event(resumed.mark, "exited", |_| true),
        json!({"exitCode": 0})
    );
    dap.finish();
}

/// Step Into Target: `stepInTargets` names the calls of the innermost
/// frame's line, and `stepIn` with one of them goes into that call,
/// running the line's others to their returns.
#[test]
fn step_in_goes_into_the_target_the_client_chose() {
    let mut dap = Dap::start("step in targets");
    let stop = stopped_at(
        &mut dap,
        "step-targets-gcc-o0",
        "c/step-targets.c",
        "targets: calls",
    );
    let thread = stop.thread;
    let frames = dap.request("stackTrace", json!({"threadId": thread}))["stackFrames"]
        .as_array()
        .expect("frames")
        .clone();
    let listed = dap.request("stepInTargets", json!({"frameId": frames[0]["id"]}));
    let targets = listed["targets"].as_array().expect("targets").clone();
    let labels = targets
        .iter()
        .map(|target| target["label"].as_str().expect("a label"))
        .collect::<Vec<_>>();
    assert_eq!(labels, ["inc", "twice", "add"], "{targets:?}");

    // A caller's line has nothing to step into.
    if let Some(caller) = frames.get(1) {
        let none = dap.request("stepInTargets", json!({"frameId": caller["id"]}));
        assert_eq!(none["targets"], json!([]));
    }

    let add = targets[2]["id"].clone();
    let sent = dap.send("stepIn", json!({"threadId": thread, "targetId": add}));
    dap.success(sent);
    assert_eq!(dap.stopped(sent.mark).reason, "step");
    let (function, line, _) = top(&mut dap, thread);
    assert_eq!(function, "add");
    assert_eq!(
        line,
        json!(line_of(&source("c/step-targets.c"), "targets: add"))
    );

    // A target belongs to the stop that listed it.
    let stale = dap.request_error("stepIn", json!({"threadId": thread, "targetId": add}));
    assert_eq!(stale, "the step-in target belongs to an earlier stop");
    dap.finish();
}
