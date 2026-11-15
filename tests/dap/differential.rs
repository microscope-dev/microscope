use std::collections::BTreeMap;
use std::process::Command;

use serde_json::{Value, json};

use crate::dap::{Configuration, Dap, Profile, fixture};

#[derive(Debug, PartialEq, Eq)]
struct Stop {
    reason: String,
    function: String,
    line: i64,

    values: BTreeMap<String, String>,
    threads: usize,
}

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

fn inspect(dap: &mut Dap, thread: i64, reason: &str) -> Stop {
    let threads = dap.request("threads", Value::Null)["threads"]
        .as_array()
        .map_or(0, Vec::len);
    let trace = dap.request("stackTrace", json!({"threadId": thread, "levels": 1}));
    let frame = &trace["stackFrames"][0];
    let scopes = dap.request("scopes", json!({"frameId": frame["id"]}));

    let mut values = BTreeMap::new();
    for scope in scopes["scopes"].as_array().into_iter().flatten() {
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
