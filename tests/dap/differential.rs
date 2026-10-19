//! Differential tests against gdb's Debug Adapter Protocol server.
//!
//! The same scripted session drives `microscope dap` and `gdb -i=dap` over the
//! same program, and each stop must agree: its function and line, the
//! arguments and locals it shows and their numeric values, what a step out
//! returned, and how many threads exist.
//! Known divergences are encoded where they arise rather than filtered.

use std::collections::BTreeMap;
use std::process::Command;

use serde_json::{Value, json};

use crate::dap::{Configuration, Dap, Profile, fixture};

/// What a client sees at one stop.
#[derive(Debug, PartialEq, Eq)]
struct Stop {
    reason: String,
    function: String,
    line: i64,

    /// Every argument and local, by name, with its comparable value, and
    /// what a step out returned as `returned`.
    values: BTreeMap<String, String>,
    threads: usize,
}

/// Drives one adapter through a function breakpoint and a list of steps,
/// recording every stop until the program exits.
fn walk(dap: &mut Dap, program: &str, function: &str, steps: &[&str]) -> Vec<Stop> {
    let started = dap.launch(
        Profile::Neovim,
        &fixture(program),
        json!({}),
        &Configuration {
            functions: vec![function.to_owned()],
            exceptions: Some(json!({"filters": []})),
            ..Configuration::default()
        },
    );
    let mut recorded = Vec::new();
    let mut mark = started.mark;

    for step in steps.iter().copied().chain(["continue"]) {
        let (kind, body) = dap.next_event(mark, &["stopped", "exited"]);
        if kind == "exited" {
            break;
        }

        let thread = body["threadId"].as_i64().expect("threadId");
        recorded.push(inspect(
            dap,
            thread,
            body["reason"].as_str().unwrap_or_default(),
        ));

        let sent = dap.send(step, json!({"threadId": thread}));
        let response = dap.response(sent);
        if response["success"] != true {
            // Recorded so the adapters must agree on refusals too.
            recorded.push(Stop {
                reason: format!("{step} refused"),
                function: String::new(),
                line: 0,
                values: BTreeMap::new(),
                threads: 0,
            });
            break;
        }

        mark = sent.mark;
    }

    recorded
}

/// The part of a value both adapters show alike: a number, which gdb
/// follows with a character's quoted form, or a boolean. Pointers,
/// records, and arrays are formatted differently, and gdb offers to expand
/// even a null pointer, so only their presence is compared.
fn comparable(value: &str) -> String {
    let number = value.split(' ').next().unwrap_or_default();
    if number.parse::<i128>().is_ok() || number.parse::<f64>().is_ok() {
        number.to_owned()
    } else if matches!(value, "true" | "false") {
        value.to_owned()
    } else {
        "…".to_owned()
    }
}

/// Reads a stop as a client shows it.
fn inspect(dap: &mut Dap, thread: i64, reason: &str) -> Stop {
    let threads = dap.request("threads", Value::Null)["threads"]
        .as_array()
        .map_or(0, Vec::len);
    let trace = dap.request("stackTrace", json!({"threadId": thread, "levels": 1}));
    let frame = &trace["stackFrames"][0];
    let scopes = dap.request("scopes", json!({"frameId": frame["id"]}));

    let mut values = BTreeMap::new();
    for scope in scopes["scopes"].as_array().into_iter().flatten() {
        // gdb shows what a step out returned in a scope of its own, and
        // microscope as a local named for the function.
        let returned = scope["name"] == "Return";
        if !returned && !matches!(scope["name"].as_str(), Some("Arguments" | "Locals")) {
            continue;
        }

        let variables = dap.request(
            "variables",
            json!({"variablesReference": scope["variablesReference"]}),
        );
        for variable in variables["variables"].as_array().into_iter().flatten() {
            let name = variable["name"].as_str().unwrap_or_default();
            let name = if returned || name.starts_with("returned ") {
                "returned"
            } else {
                name
            };

            values.insert(
                name.to_owned(),
                comparable(variable["value"].as_str().unwrap_or_default()),
            );
        }
    }

    Stop {
        reason: reason.to_owned(),
        function: frame["name"].as_str().unwrap_or_default().to_owned(),
        line: frame["line"].as_i64().unwrap_or_default(),
        values,
        threads,
    }
}

/// Runs a script through both adapters and returns their stops.
fn compare(program: &str, function: &str, steps: &[&str]) -> (Vec<Stop>, Vec<Stop>) {
    let mut ours = Dap::start(format!("microscope {program}"));
    let microscope = walk(&mut ours, program, function, steps);
    ours.finish();

    let mut reference = Dap::reference(
        format!("gdb {program}"),
        Command::new("gdb").args(["--quiet", "--nx", "-i=dap"]),
    );
    let gdb = walk(&mut reference, program, function, steps);
    reference.abandon();

    (microscope, gdb)
}

#[test]
fn stepping_stops_where_gdb_stops_with_the_same_values() {
    // From main's first line: over the inlined call, into and out of each
    // call, and over a line without one.
    let steps = [
        "next", "stepIn", "next", "next", "stepOut", "next", "next", "stepIn", "stepOut", "next",
        "stepIn", "stepOut", "next",
    ];
    for program in ["stepping-boundaries-gcc-o0", "stepping-boundaries-clang-o0"] {
        let (mut microscope, gdb) = compare(program, "main", &steps);
        assert_eq!(
            microscope.len(),
            steps.len() + 1,
            "{program}: {microscope:#?}"
        );

        // gdb names every breakpoint stop "breakpoint"; the protocol has a
        // reason for a function breakpoint's.
        assert_eq!(
            (microscope[0].reason.as_str(), gdb[0].reason.as_str()),
            ("function breakpoint", "breakpoint")
        );
        microscope[0].reason = gdb[0].reason.clone();
        assert_eq!(
            microscope, gdb,
            "{program}: microscope stops differ from gdb's"
        );
    }
}

#[test]
fn threads_stop_and_step_where_gdb_stops_them() {
    // Over a thread's creation and over joining it, which only completes
    // while the other thread runs. The worker waits at a gate the joining
    // line opens, so it is alive at every stop before that line's step, and
    // gone after it: a stop waits for an exiting thread's exit status.
    let (mut microscope, gdb) = compare(
        "thread-steps-gcc-o0",
        "main",
        &["next", "next", "next", "next"],
    );
    assert_eq!(
        microscope
            .iter()
            .map(|stop| stop.threads)
            .collect::<Vec<_>>(),
        [1, 1, 2, 2, 1]
    );
    microscope[0].reason = gdb[0].reason.clone();
    assert_eq!(microscope, gdb);

    // In a thread other than the first, and out to its caller.
    let (mut microscope, gdb) = compare(
        "thread-steps-gcc-o0",
        "worker_reached",
        &["stepOut", "next", "next"],
    );
    assert_eq!(microscope.len(), 4, "{microscope:#?}");
    assert_eq!(microscope[0].threads, 2);
    microscope[0].reason = gdb[0].reason.clone();
    assert_eq!(microscope, gdb);
}
