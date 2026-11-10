//! Every request and outside event in every phase of a session.
//!
//! Each row starts an adapter, brings its session to one phase, sends one
//! request or does one thing to the program, and then disconnects. It
//! records what the client was told: every response, the events that
//! matter to a session's phase and run state, and, for an attached
//! process, how it ended. The rows run in parallel, a few at a time.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};

use crate::dap::{Configuration, Dap, Mark, Profile, fixture};
use crate::support::ExternalProcess;

const PHASES: &[&str] = &[
    "new",
    "initialized",
    "configured",
    "launch held",
    "attach held",
    "running",
    "stopped",
    "attached",
    "core",
    "exited",
];

const EVENTS: &[&str] = &[
    "initialize",
    "launch",
    "attach core",
    "configurationDone",
    "threads",
    "stackTrace",
    "continue",
    "next",
    "pause",
    "restart",
    "terminate",
    "disconnect",
    "close input",
    "killed outside",
    "exits",
];

/// A session brought to one phase.
struct Row {
    dap: Dap,

    /// The process an attach phase attached to.
    attached: Option<ExternalProcess>,

    /// The client's id of the thread requests name.
    thread: i64,

    /// A frame of the phase's stop, whose handle a resume ends.
    frame: Option<Value>,
}

fn launch_arguments(program: &str) -> Value {
    json!({"program": fixture(program)})
}

fn core_arguments() -> Value {
    json!({"coreFile": fixture("crash-gcc-o0-segv.core"), "program": fixture("crash-gcc-o0")})
}

/// Sends a request, then waits until the session has handled it: requests
/// are handled in order, so once a `threads` sent after it is answered, a
/// request still unanswered is held.
fn send_and_settle(dap: &mut Dap, command: &str, arguments: Value) {
    dap.send(command, arguments);
    ask(dap, "threads", Value::Null);
}

/// Sends a request and returns its response, whether it succeeded or not.
fn ask(dap: &mut Dap, command: &str, arguments: Value) -> Value {
    let sent = dap.send(command, arguments);
    dap.response(sent)
}

fn phase(name: &str) -> Row {
    let mut dap = Dap::start(format!("phase {name}"));
    let mut attached = None;
    let mut thread = 1;
    match name {
        "new" => {}
        "initialized" => {
            dap.initialize(Profile::Helix);
        }
        "configured" => {
            dap.initialize(Profile::Helix);
            dap.request("configurationDone", Value::Null);
        }
        "launch held" => {
            dap.initialize(Profile::Helix);
            send_and_settle(&mut dap, "launch", launch_arguments("spin"));
        }
        "attach held" => {
            let process = ExternalProcess::spawn(&fixture("attach"));
            dap.initialize(Profile::Helix);
            send_and_settle(
                &mut dap,
                "attach",
                json!({"pid": process.process_id().get()}),
            );
            attached = Some(process);
        }
        "running" => {
            // Past its loader, so that nothing the loader does can race with
            // the row.
            let started = dap.begin(
                Profile::Helix,
                ("launch", launch_arguments("spin")),
                &Configuration {
                    functions: vec!["main".to_owned()],
                    ..Configuration::default()
                },
            );
            let stop = dap.stopped(started.mark);
            dap.request("setFunctionBreakpoints", json!({"breakpoints": []}));
            dap.request("continue", json!({"threadId": stop.thread}));
        }
        "stopped" => {
            let started = dap.begin(
                Profile::Helix,
                ("launch", launch_arguments("spin")),
                &Configuration {
                    functions: vec!["main".to_owned()],
                    ..Configuration::default()
                },
            );
            thread = dap.stopped(started.mark).thread;
        }
        "attached" => {
            let process = ExternalProcess::spawn(&fixture("attach"));
            let started = dap.begin(
                Profile::Helix,
                ("attach", json!({"pid": process.process_id().get()})),
                &Configuration::default(),
            );
            dap.event(started.mark, "process", |_| true);
            dap.event(started.mark, "thread", |_| true);
            attached = Some(process);
        }
        "core" => {
            let started = dap.begin(
                Profile::Helix,
                ("attach", core_arguments()),
                &Configuration::default(),
            );
            thread = dap.stopped(started.mark).thread;
        }
        "exited" => {
            let started = dap.begin(
                Profile::Helix,
                ("launch", launch_arguments("basic")),
                &Configuration::default(),
            );
            dap.event(started.mark, "terminated", |_| true);
        }
        _ => unreachable!("no phase {name}"),
    }

    let frame = matches!(name, "stopped" | "core").then(|| {
        dap.request("stackTrace", json!({"threadId": thread}))["stackFrames"][0]["id"].clone()
    });

    Row {
        dap,
        attached,
        thread,
        frame,
    }
}

/// The events of a request that the row waits for before it disconnects,
/// so that what it records does not depend on timing.
/// A program started is announced by its process and then its thread;
/// spin's `main` loops through its own breakpoint, so a stop there comes
/// back after each continue.
fn awaited(name: &str, event: &str) -> &'static [&'static str] {
    let live = matches!(name, "running" | "stopped" | "attached");
    match (event, name) {
        ("launch", "configured")
        | ("configurationDone", "launch held" | "attach held")
        | ("restart", "running" | "exited") => &["process", "thread"],
        ("attach core", "configured")
        | ("continue" | "next", "stopped")
        | ("pause", "running" | "attached") => &["stopped"],
        ("restart", "stopped") => &["process", "thread", "stopped"],
        ("terminate", _) if live => &["terminated"],
        ("killed outside" | "exits", _) => &["terminated"],
        _ => &[],
    }
}

/// What the client was told after `mark`: each response, and each event
/// that says where the session or its program is. Module, breakpoint and
/// program output events are left out, as their timing is the program's.
fn told(dap: &mut Dap, mark: Mark) -> String {
    let mut said = Vec::new();
    for message in dap.messages_since(mark) {
        let body = &message["body"];
        let text = match (message["type"].as_str(), message["event"].as_str()) {
            (Some("response"), _) if message["command"] == "threads" => continue,
            (Some("response"), _) if message["success"] == true => {
                format!("{} ok", message["command"].as_str().unwrap_or_default())
            }
            (Some("response"), _) => format!(
                "{} failed: {}",
                message["command"].as_str().unwrap_or_default(),
                message["message"].as_str().unwrap_or_default()
            ),
            (Some("event"), Some("stopped")) => {
                format!("stopped {}", body["reason"].as_str().unwrap_or_default())
            }
            (Some("event"), Some("exited")) => format!("exited {}", body["exitCode"]),
            (Some("event"), Some("thread")) => {
                format!("thread {}", body["reason"].as_str().unwrap_or_default())
            }
            (Some("event"), Some("process")) => {
                format!(
                    "process {}",
                    body["startMethod"].as_str().unwrap_or_default()
                )
            }
            (Some("event"), Some("output"))
                if matches!(body["category"].as_str(), Some("important" | "console")) =>
            {
                format!(
                    "said {:?}",
                    body["output"].as_str().unwrap_or_default().trim()
                )
            }
            (Some("event"), Some(event @ ("continued" | "terminated" | "initialized"))) => {
                event.to_owned()
            }
            (Some("event"), Some("capabilities")) => "capabilities".to_owned(),
            _ => continue,
        };
        said.push(text);
    }

    // Each stop is waited for here, so the harness counts it as seen.
    dap.events(mark, "stopped");
    said.join(", ")
}

/// Sends the request, or does to the program what `event` names. Returns
/// whether that ended the session.
fn act(row: &mut Row, event: &str) -> bool {
    let dap = &mut row.dap;
    let thread = row.thread;
    match event {
        "initialize" => send_and_settle(
            dap,
            "initialize",
            json!({"adapterID": "microscope", "clientID": "hx"}),
        ),
        "launch" => send_and_settle(dap, "launch", launch_arguments("spin")),
        "attach core" => send_and_settle(dap, "attach", core_arguments()),
        "configurationDone" => send_and_settle(dap, "configurationDone", Value::Null),
        "threads" => {
            ask(dap, "threads", Value::Null);
        }
        "stackTrace" => {
            ask(dap, "stackTrace", json!({"threadId": thread}));
        }
        "continue" => {
            ask(dap, "continue", json!({"threadId": thread}));
        }
        "next" => {
            ask(
                dap,
                "next",
                json!({"threadId": thread, "granularity": "instruction"}),
            );
        }
        "pause" => {
            ask(dap, "pause", json!({"threadId": thread}));
        }
        "restart" => {
            ask(dap, "restart", json!({}));
        }
        "terminate" => send_and_settle(dap, "terminate", json!({})),
        "disconnect" => return ask(dap, "disconnect", json!({}))["success"] == true,
        "close input" => return true,
        "killed outside" => {
            let pid = dap.process_id().expect("a live process");
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(i32::try_from(pid).expect("pid fits i32")),
                nix::sys::signal::Signal::SIGKILL,
            )
            .expect("kill the program");
        }
        "exits" => row
            .attached
            .as_mut()
            .expect("an attached process")
            .release(),
        _ => unreachable!("no event {event}"),
    }

    false
}

/// Whether the handle of a frame of the phase's stop is still good.
fn frame_state(dap: &mut Dap, frame: &Value) -> String {
    let response = ask(dap, "scopes", json!({"frameId": frame}));
    if response["success"] == true {
        return "live".to_owned();
    }

    let message = response["message"].as_str().unwrap_or_default();
    if message.contains("is stale") {
        "stale".to_owned()
    } else {
        message.to_owned()
    }
}

/// How an attached process ended: one the session let go waits to be
/// released, and one it killed is gone.
fn fate(mut process: ExternalProcess, released: bool) -> String {
    if !released {
        let _ = std::io::Write::write_all(&mut process.take_stdin(), b"x");
    }

    let status = process.wait();
    match (
        status.code(),
        std::os::unix::process::ExitStatusExt::signal(&status),
    ) {
        (Some(code), _) => format!(" -> process exits {code}"),
        (None, Some(signal)) => format!(" -> process killed by {signal}"),
        (None, None) => " -> process ends".to_owned(),
    }
}

/// One event in one phase: what the client was told after it, what it
/// was told when the session then ended, and how an attached process
/// ended.
fn outcome(name: &str, event: &str) -> Option<String> {
    let live = matches!(name, "running" | "stopped" | "attached");
    if (event == "killed outside" && !live) || (event == "exits" && name != "attached") {
        return None;
    }

    let mut row = phase(name);
    let mark = row.dap.mark();
    let ended = act(&mut row, event);
    let Row {
        mut dap,
        attached,
        frame,
        ..
    } = row;

    for awaited in awaited(name, event) {
        dap.event(mark, awaited, |body| {
            *awaited != "thread" || body["reason"] == "started"
        });
    }

    let mut after = told(&mut dap, mark);
    if let Some(frame) = frame.filter(|_| !ended) {
        after = format!("{after} [frame {}]", frame_state(&mut dap, &frame));
    }

    // A session ends by a disconnect, which a session that was never
    // initialized refuses; that one ends when its input closes.
    let end = dap.mark();
    if !ended && name != "new" {
        let response = ask(&mut dap, "disconnect", json!({}));
        assert_eq!(response["success"], true, "{name} / {event}: {response}");
    }
    dap.close_stdin();
    dap.finish_with(json!({}));
    let at_end = told(&mut dap, end);
    let fate = attached.map_or_else(String::new, |process| fate(process, event == "exits"));

    Some(format!("{name} / {event}: {after} | end: {at_end}{fate}"))
}

/// Every request and outside event, in each phase of a session, is
/// answered and reported as it always has been.
#[test]
fn each_request_in_each_phase_is_answered_and_reported_the_same() {
    let rows = PHASES
        .iter()
        .flat_map(|name| EVENTS.iter().map(move |event| (*name, *event)))
        .collect::<Vec<_>>();
    let results = Mutex::new(vec![None; rows.len()]);
    let next = AtomicUsize::new(0);
    let workers = std::thread::available_parallelism().map_or(4, |count| count.get().min(8));

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some((name, event)) = rows.get(index) else {
                        break;
                    };
                    let row = outcome(name, event);
                    results.lock().expect("results")[index] = Some(row);
                }
            });
        }
    });

    let actual = results
        .into_inner()
        .expect("results")
        .into_iter()
        .filter_map(|row| row.expect("every row ran"))
        .collect::<Vec<_>>();
    let rows = actual.join("\n");
    assert_eq!(
        EXPECTED,
        actual.as_slice(),
        "the session table changed; its rows are now:\n{rows}"
    );
}

/// The table's rows, as [`outcome`] writes them.
const EXPECTED: &[&str] = &[
    "new / initialize: initialize ok, initialized | end: ",
    "new / launch: launch failed: the client must send initialize before launch | end: ",
    "new / attach core: attach failed: the client must send initialize before attach | end: ",
    "new / configurationDone: configurationDone failed: the client must send initialize before configurationDone | end: ",
    "new / threads:  | end: ",
    "new / stackTrace: stackTrace failed: the client must send initialize before stackTrace | end: ",
    "new / continue: continue failed: the client must send initialize before continue | end: ",
    "new / next: next failed: the client must send initialize before next | end: ",
    "new / pause: pause failed: the client must send initialize before pause | end: ",
    "new / restart: restart failed: the client must send initialize before restart | end: ",
    "new / terminate: terminate failed: the client must send initialize before terminate | end: ",
    "new / disconnect: disconnect failed: the client must send initialize before disconnect | end: ",
    "new / close input:  | end: ",
    "initialized / initialize: initialize failed: the session is already initialized | end: disconnect ok",
    "initialized / launch:  | end: launch failed: the session ended before its configuration was done, disconnect ok",
    "initialized / attach core: capabilities | end: attach failed: the session ended before its configuration was done, disconnect ok",
    "initialized / configurationDone: configurationDone ok | end: disconnect ok",
    "initialized / threads:  | end: disconnect ok",
    "initialized / stackTrace: stackTrace failed: the program is not running | end: disconnect ok",
    "initialized / continue: continue failed: the program is not running | end: disconnect ok",
    "initialized / next: next failed: the program is not running | end: disconnect ok",
    "initialized / pause: pause failed: no program is loaded | end: disconnect ok",
    "initialized / restart: restart failed: no program is loaded | end: disconnect ok",
    "initialized / terminate: terminate ok | end: disconnect ok",
    "initialized / disconnect: disconnect ok | end: ",
    "initialized / close input:  | end: ",
    "configured / initialize: initialize failed: the session is already initialized | end: disconnect ok",
    "configured / launch: launch ok, process launch, thread started | end: said \"the program was terminated by SIGKILL (SIGKILL)\", exited 137, thread exited, terminated, disconnect ok",
    "configured / attach core: capabilities, attach ok, process attach, thread started, thread started, thread started, thread started, stopped exception | end: thread exited, thread exited, thread exited, thread exited, terminated, disconnect ok",
    "configured / configurationDone: configurationDone failed: configuration is already done | end: disconnect ok",
    "configured / threads:  | end: disconnect ok",
    "configured / stackTrace: stackTrace failed: the program is not running | end: disconnect ok",
    "configured / continue: continue failed: the program is not running | end: disconnect ok",
    "configured / next: next failed: the program is not running | end: disconnect ok",
    "configured / pause: pause failed: no program is loaded | end: disconnect ok",
    "configured / restart: restart failed: no program is loaded | end: disconnect ok",
    "configured / terminate: terminate ok | end: disconnect ok",
    "configured / disconnect: disconnect ok | end: ",
    "configured / close input:  | end: ",
    "launch held / initialize: initialize failed: the session is already initialized | end: launch failed: the session ended before its configuration was done, disconnect ok",
    "launch held / launch: launch failed: the session already has a program; start a new session for another | end: launch failed: the session ended before its configuration was done, disconnect ok",
    "launch held / attach core: attach failed: the session already has a program; start a new session for another | end: launch failed: the session ended before its configuration was done, disconnect ok",
    "launch held / configurationDone: configurationDone ok, launch ok, process launch, thread started | end: said \"the program was terminated by SIGKILL (SIGKILL)\", exited 137, thread exited, terminated, disconnect ok",
    "launch held / threads:  | end: launch failed: the session ended before its configuration was done, disconnect ok",
    "launch held / stackTrace: stackTrace failed: the program is not running | end: launch failed: the session ended before its configuration was done, disconnect ok",
    "launch held / continue: continue failed: the program is not running | end: launch failed: the session ended before its configuration was done, disconnect ok",
    "launch held / next: next failed: the program is not running | end: launch failed: the session ended before its configuration was done, disconnect ok",
    "launch held / pause: pause failed: the program is not running | end: launch failed: the session ended before its configuration was done, disconnect ok",
    "launch held / restart: restart failed: the program has not started yet, so there is nothing to restart | end: launch failed: the session ended before its configuration was done, disconnect ok",
    "launch held / terminate: launch failed: the session was terminated before its configuration was done, terminated, terminate ok | end: disconnect ok",
    "launch held / disconnect: launch failed: the session ended before its configuration was done, disconnect ok | end: ",
    "launch held / close input:  | end: launch failed: the session ended before its configuration was done",
    "attach held / initialize: initialize failed: the session is already initialized | end: attach failed: the session ended before its configuration was done, terminated, disconnect ok -> process exits 23",
    "attach held / launch: launch failed: the session already has a program; start a new session for another | end: attach failed: the session ended before its configuration was done, terminated, disconnect ok -> process exits 23",
    "attach held / attach core: attach failed: the session already has a program; start a new session for another | end: attach failed: the session ended before its configuration was done, terminated, disconnect ok -> process exits 23",
    "attach held / configurationDone: configurationDone ok, attach ok, process attach, thread started | end: terminated, disconnect ok -> process exits 23",
    "attach held / threads:  | end: attach failed: the session ended before its configuration was done, terminated, disconnect ok -> process exits 23",
    "attach held / stackTrace: stackTrace failed: the program is not running | end: attach failed: the session ended before its configuration was done, terminated, disconnect ok -> process exits 23",
    "attach held / continue: continue failed: the program is not running | end: attach failed: the session ended before its configuration was done, terminated, disconnect ok -> process exits 23",
    "attach held / next: next failed: the program is not running | end: attach failed: the session ended before its configuration was done, terminated, disconnect ok -> process exits 23",
    "attach held / pause: pause ok | end: attach failed: the session ended before its configuration was done, terminated, disconnect ok -> process exits 23",
    "attach held / restart: restart failed: the program has not started yet, so there is nothing to restart | end: attach failed: the session ended before its configuration was done, terminated, disconnect ok -> process exits 23",
    "attach held / terminate: attach failed: the session was terminated before its configuration was done, terminated, terminate ok | end: terminated, disconnect ok -> process exits 23",
    "attach held / disconnect: attach failed: the session ended before its configuration was done, terminated, disconnect ok | end:  -> process exits 23",
    "attach held / close input:  | end: attach failed: the session ended before its configuration was done, terminated -> process exits 23",
    "running / initialize: initialize failed: the session is already initialized | end: said \"the program was terminated by SIGKILL (SIGKILL)\", exited 137, thread exited, terminated, disconnect ok",
    "running / launch: launch failed: the session already has a program; start a new session for another | end: said \"the program was terminated by SIGKILL (SIGKILL)\", exited 137, thread exited, terminated, disconnect ok",
    "running / attach core: attach failed: the session already has a program; start a new session for another | end: said \"the program was terminated by SIGKILL (SIGKILL)\", exited 137, thread exited, terminated, disconnect ok",
    "running / configurationDone: configurationDone failed: configuration is already done | end: said \"the program was terminated by SIGKILL (SIGKILL)\", exited 137, thread exited, terminated, disconnect ok",
    "running / threads:  | end: said \"the program was terminated by SIGKILL (SIGKILL)\", exited 137, thread exited, terminated, disconnect ok",
    "running / stackTrace: stackTrace failed: notStopped | end: said \"the program was terminated by SIGKILL (SIGKILL)\", exited 137, thread exited, terminated, disconnect ok",
    "running / continue: continue failed: notStopped | end: said \"the program was terminated by SIGKILL (SIGKILL)\", exited 137, thread exited, terminated, disconnect ok",
    "running / next: next failed: notStopped | end: said \"the program was terminated by SIGKILL (SIGKILL)\", exited 137, thread exited, terminated, disconnect ok",
    "running / pause: pause ok, stopped pause | end: said \"the program was terminated by SIGKILL (SIGKILL)\", exited 137, thread exited, terminated, disconnect ok",
    "running / restart: said \"the program was terminated by SIGKILL (SIGKILL)\", exited 137, thread exited, process launch, thread started, restart ok | end: said \"the program was terminated by SIGKILL (SIGKILL)\", exited 137, thread exited, terminated, disconnect ok",
    "running / terminate: terminate ok, said \"the program was terminated by SIGTERM (SIGTERM)\", exited 143, thread exited, terminated | end: disconnect ok",
    "running / disconnect: said \"the program was terminated by SIGKILL (SIGKILL)\", exited 137, thread exited, terminated, disconnect ok | end: ",
    "running / close input:  | end: said \"the program was terminated by SIGKILL (SIGKILL)\", exited 137, thread exited, terminated",
    "running / killed outside: said \"the program was terminated by SIGKILL (SIGKILL)\", exited 137, thread exited, terminated | end: disconnect ok",
    "stopped / initialize: initialize failed: the session is already initialized [frame live] | end: said \"the program was terminated by SIGKILL (SIGKILL)\", exited 137, thread exited, terminated, disconnect ok",
    "stopped / launch: launch failed: the session already has a program; start a new session for another [frame live] | end: said \"the program was terminated by SIGKILL (SIGKILL)\", exited 137, thread exited, terminated, disconnect ok",
    "stopped / attach core: attach failed: the session already has a program; start a new session for another [frame live] | end: said \"the program was terminated by SIGKILL (SIGKILL)\", exited 137, thread exited, terminated, disconnect ok",
    "stopped / configurationDone: configurationDone failed: configuration is already done [frame live] | end: said \"the program was terminated by SIGKILL (SIGKILL)\", exited 137, thread exited, terminated, disconnect ok",
    "stopped / threads:  [frame live] | end: said \"the program was terminated by SIGKILL (SIGKILL)\", exited 137, thread exited, terminated, disconnect ok",
    "stopped / stackTrace: stackTrace ok [frame live] | end: said \"the program was terminated by SIGKILL (SIGKILL)\", exited 137, thread exited, terminated, disconnect ok",
    "stopped / continue: continue ok, stopped function breakpoint [frame stale] | end: said \"the program was terminated by SIGKILL (SIGKILL)\", exited 137, thread exited, terminated, disconnect ok",
    "stopped / next: continued, next ok, stopped step [frame stale] | end: said \"the program was terminated by SIGKILL (SIGKILL)\", exited 137, thread exited, terminated, disconnect ok",
    "stopped / pause: pause ok [frame live] | end: said \"the program was terminated by SIGKILL (SIGKILL)\", exited 137, thread exited, terminated, disconnect ok",
    "stopped / restart: said \"the program was terminated by SIGKILL (SIGKILL)\", exited 137, thread exited, process launch, thread started, restart ok, stopped function breakpoint [frame stale] | end: said \"the program was terminated by SIGKILL (SIGKILL)\", exited 137, thread exited, terminated, disconnect ok",
    "stopped / terminate: terminate ok, continued, said \"the program was terminated by SIGTERM (SIGTERM)\", exited 143, thread exited, terminated [frame the program is not running] | end: disconnect ok",
    "stopped / disconnect: said \"the program was terminated by SIGKILL (SIGKILL)\", exited 137, thread exited, terminated, disconnect ok | end: ",
    "stopped / close input:  | end: said \"the program was terminated by SIGKILL (SIGKILL)\", exited 137, thread exited, terminated",
    "stopped / killed outside: said \"the program was terminated by SIGKILL (SIGKILL)\", exited 137, thread exited, terminated [frame the program is not running] | end: disconnect ok",
    "attached / initialize: initialize failed: the session is already initialized | end: terminated, disconnect ok -> process exits 23",
    "attached / launch: launch failed: the session already has a program; start a new session for another | end: terminated, disconnect ok -> process exits 23",
    "attached / attach core: attach failed: the session already has a program; start a new session for another | end: terminated, disconnect ok -> process exits 23",
    "attached / configurationDone: configurationDone failed: configuration is already done | end: terminated, disconnect ok -> process exits 23",
    "attached / threads:  | end: terminated, disconnect ok -> process exits 23",
    "attached / stackTrace: stackTrace failed: notStopped | end: terminated, disconnect ok -> process exits 23",
    "attached / continue: continue failed: notStopped | end: terminated, disconnect ok -> process exits 23",
    "attached / next: next failed: notStopped | end: terminated, disconnect ok -> process exits 23",
    "attached / pause: pause ok, stopped pause | end: terminated, disconnect ok -> process exits 23",
    "attached / restart: restart failed: only a launched program can be restarted; start a new session instead | end: terminated, disconnect ok -> process exits 23",
    "attached / terminate: terminate ok, said \"the program was terminated by SIGTERM (SIGTERM)\", exited 143, thread exited, terminated | end: disconnect ok -> process killed by 15",
    "attached / disconnect: terminated, disconnect ok | end:  -> process exits 23",
    "attached / close input:  | end: terminated -> process exits 23",
    "attached / killed outside: said \"the program was terminated by SIGKILL (SIGKILL)\", exited 137, thread exited, terminated | end: disconnect ok -> process killed by 9",
    "attached / exits: exited 23, thread exited, terminated | end: disconnect ok -> process exits 23",
    "core / initialize: initialize failed: the session is already initialized [frame live] | end: thread exited, thread exited, thread exited, thread exited, terminated, disconnect ok",
    "core / launch: launch failed: the session already has a program; start a new session for another [frame live] | end: thread exited, thread exited, thread exited, thread exited, terminated, disconnect ok",
    "core / attach core: attach failed: the session already has a program; start a new session for another [frame live] | end: thread exited, thread exited, thread exited, thread exited, terminated, disconnect ok",
    "core / configurationDone: configurationDone failed: configuration is already done [frame live] | end: thread exited, thread exited, thread exited, thread exited, terminated, disconnect ok",
    "core / threads:  [frame live] | end: thread exited, thread exited, thread exited, thread exited, terminated, disconnect ok",
    "core / stackTrace: stackTrace ok [frame live] | end: thread exited, thread exited, thread exited, thread exited, terminated, disconnect ok",
    "core / continue: continue failed: a core dump cannot run, be changed, or have breakpoints [frame live] | end: thread exited, thread exited, thread exited, thread exited, terminated, disconnect ok",
    "core / next: next failed: a core dump cannot run, be changed, or have breakpoints [frame live] | end: thread exited, thread exited, thread exited, thread exited, terminated, disconnect ok",
    "core / pause: pause failed: a core dump cannot run, be changed, or have breakpoints [frame live] | end: thread exited, thread exited, thread exited, thread exited, terminated, disconnect ok",
    "core / restart: restart failed: only a launched program can be restarted; start a new session instead [frame live] | end: thread exited, thread exited, thread exited, thread exited, terminated, disconnect ok",
    "core / terminate: terminate failed: a core dump cannot run, be changed, or have breakpoints [frame live] | end: thread exited, thread exited, thread exited, thread exited, terminated, disconnect ok",
    "core / disconnect: thread exited, thread exited, thread exited, thread exited, terminated, disconnect ok | end: ",
    "core / close input:  | end: thread exited, thread exited, thread exited, thread exited, terminated",
    "exited / initialize: initialize failed: the session is already initialized | end: disconnect ok",
    "exited / launch: launch failed: the session already has a program; start a new session for another | end: disconnect ok",
    "exited / attach core: attach failed: the session already has a program; start a new session for another | end: disconnect ok",
    "exited / configurationDone: configurationDone failed: configuration is already done | end: disconnect ok",
    "exited / threads:  | end: disconnect ok",
    "exited / stackTrace: stackTrace failed: the program is not running | end: disconnect ok",
    "exited / continue: continue failed: the program is not running | end: disconnect ok",
    "exited / next: next failed: the program is not running | end: disconnect ok",
    "exited / pause: pause failed: the program is not running | end: disconnect ok",
    "exited / restart: process launch, thread started, restart ok | end: said \"the program was terminated by SIGKILL (SIGKILL)\", exited 137, thread exited, terminated, disconnect ok",
    "exited / terminate: terminate ok | end: disconnect ok",
    "exited / disconnect: disconnect ok | end: ",
    "exited / close input:  | end: ",
];
