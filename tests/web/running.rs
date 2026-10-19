//! Running and stepping a program from the page, and the stacks, sources,
//! and breakpoints it shows along the way.

use serde_json::{Value, json};

use crate::support::Scenario;
use crate::web::{Client, Web};

fn fixture(name: &str) -> String {
    Scenario::fixture(name).display().to_string()
}

/// The fixture source file whose path ends with `name`.
fn source_named(files: &Value, name: &str) -> String {
    files["files"]
        .as_array()
        .expect("files")
        .iter()
        .filter_map(Value::as_str)
        .find(|path| path.ends_with(&format!("/{name}")))
        .unwrap_or_else(|| panic!("no {name} in {files}"))
        .to_owned()
}

/// Steps `thread` at `stop` and waits for the stop it ends in.
async fn step(tab: &mut Client, stop: &Value, thread: &Value, kind: &str) -> Value {
    tab.ok(
        "step",
        json!({"stop": stop, "thread": thread, "kind": kind}),
    )
    .await;
    tab.state(&format!("a step {kind}"), |state| {
        state["inferior"]["state"] == "stopped" && state["inferior"]["stop"] != *stop
    })
    .await
}

#[tokio::test]
async fn a_breakpoint_set_before_running_stops_there_and_steps_walk_the_source() {
    let web = Web::start("steps", &[&fixture("basic")]);
    let mut tab = web.control("tab").await;
    tab.state("the program loaded", |state| state["session"].is_string())
        .await;

    // Sources come from the debug information before the program runs.
    let files = tab.ok("sources", json!(null)).await;
    let path = source_named(&files, "basic.c");

    // A page opens a program that has not run on its main function.
    assert_eq!(files["entry"]["path"], path.as_str());
    assert_eq!(files["entry"]["line"], 9);
    let source = tab.ok("source", json!({"path": path})).await;
    assert!(
        source["text"]
            .as_str()
            .is_some_and(|text| text.contains("uint64_t first = breakpoint_target();")),
        "{source}"
    );
    let breakable = source["breakable"].as_array().expect("lines");
    assert!(breakable.contains(&json!(10)), "{source}");
    assert!(!breakable.contains(&json!(2)), "{source}");

    let id = tab
        .ok("addBreakpoint", json!({"location": format!("{path}:10")}))
        .await["id"]
        .clone();
    let set = tab
        .state("the breakpoint", |state| {
            state["breakpoints"][0]["id"] == id
        })
        .await;
    let breakpoint = &set["breakpoints"][0];
    assert_eq!(breakpoint["places"][0]["line"], 10, "{breakpoint}");
    assert_eq!(breakpoint["places"][0]["path"], path.as_str());

    tab.ok("continue", json!({})).await;
    let stopped = tab.inferior("stopped").await;
    let inferior = &stopped["inferior"];
    assert_eq!(inferior["reason"]["kind"], "breakpoint");
    assert_eq!(inferior["place"]["function"], "main");
    assert_eq!(inferior["place"]["line"], 10);
    assert_eq!(stopped["breakpoints"][0]["hits"], 1);
    let entry = stopped["stops"].as_array().expect("stops").last().cloned();
    assert_eq!(
        entry.as_ref().map(|entry| &entry["stop"]),
        Some(&inferior["stop"])
    );
    assert_eq!(
        entry.as_ref().map(|entry| &entry["by"]),
        Some(&json!("tester"))
    );

    let (stop, thread) = (inferior["stop"].clone(), inferior["thread"].clone());
    let trace = tab
        .ok("backtrace", json!({"stop": stop, "thread": thread}))
        .await;
    let top = &trace["frames"][0];
    assert_eq!(top["name"], "main");
    assert_eq!(top["source"]["path"], path.as_str());
    assert_eq!(top["source"]["line"], 10);

    // Into the call, out again, and over the next line.
    let into = step(&mut tab, &stop, &thread, "into").await;
    assert_eq!(
        into["inferior"]["place"]["function"], "breakpoint_target",
        "{into}"
    );
    let stop = into["inferior"]["stop"].clone();
    let out = step(&mut tab, &stop, &thread, "out").await;
    assert_eq!(out["inferior"]["place"]["function"], "main", "{out}");
    let stop = out["inferior"]["stop"].clone();
    let over = step(&mut tab, &stop, &thread, "over").await;
    assert_eq!(over["inferior"]["reason"]["kind"], "step");
    assert_eq!(over["inferior"]["place"]["line"], 11, "{over}");

    // An earlier stop can no longer be stepped.
    let (kind, _) = tab
        .request(
            "step",
            json!({"stop": stop, "thread": thread, "kind": "over"}),
        )
        .await
        .expect_err("a stale step");
    assert_eq!(kind, "staleStop");

    tab.save_traffic("steps");
}

#[tokio::test]
async fn breakpoints_change_their_conditions_and_go_before_the_program_runs() {
    let web = Web::start("breakpoints", &[&fixture("basic")]);
    let mut tab = web.control("tab").await;
    tab.state("the program loaded", |state| state["session"].is_string())
        .await;
    let id = tab
        .ok("addBreakpoint", json!({"location": "breakpoint_target"}))
        .await["id"]
        .clone();
    tab.ok(
        "editBreakpoint",
        json!({"id": id, "condition": "microscope_value == 0", "hitCondition": ">=2"}),
    )
    .await;
    let changed = tab
        .state("the condition", |state| {
            state["breakpoints"][0]["condition"] == "microscope_value == 0"
        })
        .await;
    assert_eq!(changed["breakpoints"][0]["hitCondition"], ">=2");
    let (kind, message) = tab
        .request("editBreakpoint", json!({"id": id, "hitCondition": "2"}))
        .await
        .expect_err("an ambiguous hit condition");
    assert_eq!(kind, "invalid", "{message}");
    tab.ok("removeBreakpoint", json!({"id": id})).await;
    tab.state("no breakpoints", |state| {
        state["breakpoints"].as_array().is_some_and(Vec::is_empty)
    })
    .await;
    let (kind, _) = tab
        .request("addBreakpoint", json!({"location": "no_such_function"}))
        .await
        .expect_err("a function no module has");
    assert_eq!(kind, "failed");

    // Python code loads only as the program imports it, so a Python
    // breakpoint waits for it rather than being refused.
    for location in ["app.py:12", "py:main"] {
        tab.ok("addBreakpoint", json!({"location": location})).await;
    }

    let waiting = tab
        .state("the Python breakpoints", |state| {
            state["breakpoints"]
                .as_array()
                .is_some_and(|all| all.len() == 2)
        })
        .await;

    for breakpoint in waiting["breakpoints"].as_array().into_iter().flatten() {
        assert_eq!(breakpoint["places"], json!([]), "{breakpoint}");
    }
    tab.save_traffic("breakpoints");
}

#[tokio::test]
async fn a_logpoint_writes_its_message_instead_of_stopping() {
    let web = Web::start("logpoint", &[&fixture("hit-counts-gcc-o0")]);
    let mut tab = web.control("tab").await;
    tab.state("the program loaded", |state| state["session"].is_string())
        .await;
    let id = tab
        .ok(
            "addBreakpoint",
            json!({"location": "counted", "logMessage": "{call}", "hitCondition": "%20"}),
        )
        .await["id"]
        .clone();

    // The message changes in place: the same breakpoint, never a second one.
    tab.ok(
        "editBreakpoint",
        json!({"id": id, "logMessage": "call {call}", "hitCondition": "%20"}),
    )
    .await;
    let changed = tab
        .state("the new message", |state| {
            state["breakpoints"][0]["logMessage"] == "call {call}"
        })
        .await;
    assert_eq!(changed["breakpoints"].as_array().map(Vec::len), Some(1));
    assert_eq!(changed["breakpoints"][0]["id"], id);
    tab.ok("continue", json!({})).await;
    let log = tab.output_until("log", "call 40").await;
    assert_eq!(log.lines().count(), 2, "{log}");
    assert!(log.contains("call 20\n"), "{log}");
    let exited = tab.inferior("exited").await;
    assert_eq!(exited["breakpoints"][0]["hits"], 40);
}

#[tokio::test]
async fn what_the_console_types_is_the_programs_input() {
    let web = Web::start("input", &["--run", &fixture("output-streams")]);
    let mut tab = web.control("tab").await;

    // The program flushes these lines, then waits to read.
    tab.output_until("stdout", "out 3").await;
    tab.ok("input", json!({"text": "y\n"})).await;
    tab.output_until("stdout", "stdin: data").await;
    tab.inferior("exited").await;

    // Each run gets its own input, which closing ends.
    tab.ok("continue", json!({})).await;
    tab.output_until("stdout", "out 3").await;
    tab.ok("input", json!({"text": "", "eof": true})).await;
    tab.output_until("stdout", "stdin: eof").await;
}

#[tokio::test]
async fn a_viewer_reads_stacks_and_sources_but_cannot_step_or_break() {
    let web = Web::start("viewer", &["--stop-at-entry", "--run", &fixture("basic")]);
    let mut owner = web.control("owner").await;
    let link = owner.ok("share", json!({"role": "view", "to": "/"})).await["url"]
        .as_str()
        .expect("a link")
        .to_owned();
    let mut viewer = web.joining("viewer", &link).await;
    let stopped = viewer.inferior("stopped").await;
    let (stop, thread) = (
        stopped["inferior"]["stop"].clone(),
        stopped["inferior"]["thread"].clone(),
    );
    let trace = viewer
        .ok("backtrace", json!({"stop": stop, "thread": thread}))
        .await;
    assert!(!trace["frames"].as_array().expect("frames").is_empty());
    let files = viewer.ok("sources", json!(null)).await;
    viewer
        .ok("source", json!({"path": source_named(&files, "basic.c")}))
        .await;

    for (method, params) in [
        (
            "step",
            json!({"stop": stop, "thread": thread, "kind": "over"}),
        ),
        ("addBreakpoint", json!({"location": "main"})),
        ("input", json!({"text": "x"})),
    ] {
        let (kind, _) = viewer
            .request(method, params)
            .await
            .expect_err("a viewer's change");
        assert_eq!(kind, "forbidden", "{method}");
    }

    // Only files the debug information names can be read.
    let (kind, _) = viewer
        .request("source", json!({"path": "/etc/passwd"}))
        .await
        .expect_err("a file no module names");
    assert_eq!(kind, "invalid");

    // Where each person looks is part of everyone's presence.
    viewer
        .ok(
            "setFocus",
            json!({"focus": {"url": "/s/x/stop/1", "label": "frame 0, main"}}),
        )
        .await;
    owner
        .expect("the viewer's focus", |message| {
            message["type"] == "presence"
                && message["people"].as_array().is_some_and(|people| {
                    people
                        .iter()
                        .any(|person| person["focus"]["label"] == "frame 0, main")
                })
        })
        .await;

    // Another person's focus and a link's target are opened by others, so
    // neither may lead off this page: a browser reads `//` and `/\` as a host.
    for url in [
        "//elsewhere.example/",
        "/\\elsewhere.example/",
        "https://elsewhere.example/",
    ] {
        let (kind, _) = viewer
            .request("setFocus", json!({"focus": {"url": url, "label": "x"}}))
            .await
            .expect_err("a focus off the page");
        assert_eq!(kind, "invalid", "{url}");
        let (kind, _) = viewer
            .request("share", json!({"role": "view", "to": url}))
            .await
            .expect_err("a link off the page");
        assert_eq!(kind, "invalid", "{url}");
    }
}

/// Breakpoints in code without debug information are placed and shown by
/// their symbols: an indirect function's at the implementation its
/// resolver chose.
#[tokio::test]
async fn breakpoints_without_debug_information_show_the_symbols_they_stop_in() {
    let web = Web::start("symbols", &[&fixture("measure-gcc-nodebug")]);
    let mut tab = web.control("tab").await;
    tab.state("the program loaded", |state| state["session"].is_string())
        .await;
    let measure = tab
        .ok("addBreakpoint", json!({"location": "measure"}))
        .await["id"]
        .clone();
    let set = tab
        .state("the breakpoint", |state| {
            state["breakpoints"][0]["id"] == measure
        })
        .await;
    assert_eq!(set["breakpoints"][0]["places"][0]["function"], "measure");
    tab.ok("continue", json!({})).await;
    let stopped = tab.inferior("stopped").await;
    assert_eq!(stopped["inferior"]["place"]["function"], "measure");

    let strlen = tab.ok("addBreakpoint", json!({"location": "strlen"})).await["id"].clone();
    let set = tab
        .state("the breakpoint", |state| {
            state["breakpoints"][1]["id"] == strlen
        })
        .await;
    let places = set["breakpoints"][1]["places"]
        .as_array()
        .expect("places")
        .iter()
        .filter_map(|place| place["function"].as_str())
        .collect::<Vec<_>>();
    assert!(
        places.iter().any(|name| name.starts_with("__strlen_")),
        "{places:?}"
    );

    let stop = stopped["inferior"]["stop"].clone();
    tab.ok("continue", json!({"stop": stop})).await;
    let stopped = tab
        .state("the stop in strlen", |state| {
            state["inferior"]["state"] == "stopped" && state["inferior"]["stop"] != stop
        })
        .await;
    let function = stopped["inferior"]["place"]["function"]
        .as_str()
        .unwrap_or_default();
    assert!(function.starts_with("__strlen_"), "{stopped}");
}

/// A tab moves a thread, without running it, to a line of its function, and
/// every tab follows it to a new stop there; the innermost frame's
/// registers change through their rows.
#[tokio::test]
async fn a_thread_jumps_to_a_line_and_registers_change_through_their_rows() {
    let web = Web::start("jump", &[&fixture("jump")]);
    let mut tab = web.control("tab").await;
    tab.state("the program loaded", |state| state["session"].is_string())
        .await;
    let files = tab.ok("sources", json!(null)).await;
    let path = source_named(&files, "jump.c");
    let line = |marker: &str| {
        crate::support::source_line("tests/fixtures/c/jump.c", &format!("jump: {marker}"))
    };

    tab.ok(
        "addBreakpoint",
        json!({"location": format!("{path}:{}", line("start"))}),
    )
    .await;
    tab.ok("continue", json!({})).await;
    let stopped = tab.inferior("stopped").await;
    let (stop, thread) = (
        stopped["inferior"]["stop"].clone(),
        stopped["inferior"]["thread"].clone(),
    );

    let (kind, message) = tab
        .request(
            "jump",
            json!({"stop": stop, "thread": thread, "location": "elsewhere"}),
        )
        .await
        .expect_err("a jump out of the function");
    assert!(
        message.contains("has no code in the function"),
        "{kind}: {message}"
    );
    tab.ok(
        "jump",
        json!({"stop": stop, "thread": thread, "location": format!("{path}:{}", line("target"))}),
    )
    .await;
    let moved = tab
        .state("the jump", |state| {
            state["inferior"]["state"] == "stopped" && state["inferior"]["stop"] != stop
        })
        .await;
    assert_eq!(moved["inferior"]["reason"]["kind"], "jump", "{moved}");
    assert_eq!(
        moved["inferior"]["place"]["line"],
        line("target"),
        "{moved}"
    );
    let (kind, _) = tab
        .request(
            "jump",
            json!({"stop": stop, "thread": thread, "location": format!("{path}:{}", line("start"))}),
        )
        .await
        .expect_err("a jump from an earlier stop");
    assert_eq!(kind, "staleStop");

    let at = json!({"stop": moved["inferior"]["stop"], "thread": thread, "frame": 0});
    let mut set = at.clone();
    set["path"] = "$rbx".into();
    set["value"] = "0x2a".into();
    tab.ok("setValue", set).await;

    let registers = tab.ok("registers", at).await;
    let rbx = registers["registers"]
        .as_array()
        .expect("registers")
        .iter()
        .find(|register| register["name"] == "rbx")
        .expect("rbx")
        .clone();
    assert_eq!(rbx["value"], "0x000000000000002a", "{rbx}");

    // Only `status += 100` ran.
    tab.ok("continue", json!({"stop": moved["inferior"]["stop"]}))
        .await;
    let exited = tab.inferior("exited").await;
    assert!(
        exited["inferior"].to_string().contains("with status 100"),
        "{exited}"
    );
}

/// A step into one call of a line goes into that call, running the
/// line's others to their returns.
#[tokio::test]
async fn a_step_goes_into_the_call_the_tab_chose() {
    let web = Web::start("step targets", &[&fixture("step-targets-gcc-o0")]);
    let mut tab = web.control("tab").await;
    tab.state("the program loaded", |state| state["session"].is_string())
        .await;
    let line = crate::support::source_line("tests/fixtures/c/step-targets.c", "targets: calls");
    tab.ok(
        "addBreakpoint",
        json!({"location": format!("step-targets.c:{line}")}),
    )
    .await;
    tab.ok("continue", json!({})).await;
    let stopped = tab.inferior("stopped").await;
    let (stop, thread) = (
        stopped["inferior"]["stop"].clone(),
        stopped["inferior"]["thread"].clone(),
    );
    let targets = tab
        .ok("stepTargets", json!({"stop": stop, "thread": thread}))
        .await;
    let calls = targets["calls"].as_array().expect("calls").clone();
    let callees = calls
        .iter()
        .map(|call| call["callee"].as_str().expect("a callee"))
        .collect::<Vec<_>>();
    assert_eq!(callees, ["inc", "twice", "add"], "{targets}");

    tab.ok(
        "step",
        json!({"stop": stop, "thread": thread, "kind": "into", "call": calls[2]["call"]}),
    )
    .await;
    let into = tab
        .state("the step into add", |state| {
            state["inferior"]["state"] == "stopped" && state["inferior"]["stop"] != stop
        })
        .await;
    assert_eq!(into["inferior"]["place"]["function"], "add", "{into}");
    assert_eq!(into["inferior"]["reason"]["kind"], "step", "{into}");
}
