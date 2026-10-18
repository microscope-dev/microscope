//! Values in every supported language as a client sees them.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::dap::{Configuration, Dap, Profile, fixture, line_of, source};

/// Launches a program to its first stop, returning the stopped thread and
/// its innermost frame.
fn stop(dap: &mut Dap, program: &str, configuration: &Configuration) -> (i64, Value) {
    let started = dap.launch(Profile::VsCode, &fixture(program), json!({}), configuration);
    let stop = dap.stopped(started.mark);
    assert!(stop.reason.contains("breakpoint"), "{program}: {stop:?}");
    let trace = dap.request("stackTrace", json!({"threadId": stop.thread, "levels": 1}));
    (stop.thread, trace["stackFrames"][0].clone())
}

/// The variables of a frame's argument and local scopes, by name.
fn frame_variables(dap: &mut Dap, frame: &Value) -> BTreeMap<String, Value> {
    let scopes = dap.request("scopes", json!({"frameId": frame["id"]}));
    let mut variables = BTreeMap::new();
    for scope in scopes["scopes"].as_array().expect("scopes") {
        if matches!(scope["name"].as_str(), Some("Arguments" | "Locals")) {
            let listed = dap.request(
                "variables",
                json!({"variablesReference": scope["variablesReference"]}),
            );
            for variable in listed["variables"].as_array().expect("variables") {
                variables.insert(
                    variable["name"].as_str().expect("name").to_owned(),
                    variable.clone(),
                );
            }
        }
    }

    variables
}

fn finish_running(mut dap: Dap, thread: i64) {
    let resumed = dap.send("continue", json!({"threadId": thread}));
    dap.success(resumed);
    assert_eq!(
        dap.event(resumed.mark, "exited", |_| true),
        json!({"exitCode": 0})
    );
    dap.finish();
}

#[test]
fn scalars_read_alike_in_cpp_rust_zig_and_go() {
    let snake = [
        "flag",
        "signed_value",
        "unsigned_value",
        "single",
        "double_precision",
        "local_signed",
    ];
    let camel = [
        "flag",
        "signedValue",
        "unsignedValue",
        "single",
        "doublePrecision",
        "localSigned",
    ];
    for (program, path, line, names) in [
        ("variables-cpp-gcc-o0", "cpp/variables.cpp", 27, snake),
        ("variables-cpp-clang-o0", "cpp/variables.cpp", 27, snake),
        ("variables-rust-o0", "rust/variables.rs", 33, snake),
        ("variables-zig-o0", "zig/variables.zig", 37, snake),
        ("variables-go-o0", "go/variables/main.go", 49, camel),
    ] {
        let mut dap = Dap::start(program);
        let (thread, frame) = stop(
            &mut dap,
            program,
            &Configuration {
                sources: vec![(source(path), vec![line])],
                ..Configuration::default()
            },
        );
        let variables = frame_variables(&mut dap, &frame);
        let values = names
            .iter()
            .map(|name| {
                variables
                    .get(*name)
                    .unwrap_or_else(|| panic!("{program} shows no {name}: {variables:?}"))["value"]
                    .clone()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            values,
            ["true", "-42", "42", "1.25", "-2.5", "-41"].map(Value::from),
            "{program}"
        );
        finish_running(dap, thread);
    }
}

/// Expands every value under a reference, to a bounded depth, and checks
/// that each child's `evaluateName` evaluates to the value shown for it.
fn assert_children_evaluate(
    dap: &mut Dap,
    frame: &Value,
    reference: &Value,
    depth: usize,
    checked: &mut usize,
) {
    let children = dap.request(
        "variables",
        json!({"variablesReference": reference, "start": 0, "count": 8}),
    );
    for child in children["variables"].as_array().expect("variables") {
        if let Some(expression) = child["evaluateName"].as_str() {
            let evaluated = dap.request(
                "evaluate",
                json!({"expression": expression, "frameId": frame["id"], "context": "watch"}),
            );
            assert_eq!(
                evaluated["result"], child["value"],
                "{expression} evaluates to something other than its row shows"
            );
            *checked += 1;
        }

        if depth > 0
            && child["variablesReference"]
                .as_i64()
                .is_some_and(|id| id > 0)
        {
            assert_children_evaluate(dap, frame, &child["variablesReference"], depth - 1, checked);
        }
    }
}

#[test]
fn records_and_enums_expand_and_their_members_evaluate_to_what_they_show() {
    for (program, function) in [
        ("records-cpp-gcc-o0", "inspect_records"),
        ("records-rust-o0", "inspect_records"),
        ("records-zig-o0", "inspectRecords"),
        ("records-go-o0", "main.inspectRecords"),
        ("enums-cpp-gcc-o0", "inspect_enums"),
        ("enums-rust-o0", "inspect_enum"),
        ("enums-zig-o0", "inspectEnums"),
        ("enums-go-o0", "main.inspectEnums"),
    ] {
        let mut dap = Dap::start(program);
        let (thread, frame) = stop(
            &mut dap,
            program,
            &Configuration {
                functions: vec![function.to_owned()],
                ..Configuration::default()
            },
        );
        let variables = frame_variables(&mut dap, &frame);
        assert!(!variables.is_empty(), "{program} shows no variables");
        let mut checked = 0;
        for variable in variables.values() {
            if variable["variablesReference"]
                .as_i64()
                .is_some_and(|id| id > 0)
            {
                assert_children_evaluate(
                    &mut dap,
                    &frame,
                    &variable["variablesReference"],
                    2,
                    &mut checked,
                );
            }
        }

        assert!(checked > 0, "{program} has no members to evaluate");
        finish_running(dap, thread);
    }
}

#[test]
fn step_out_shows_what_the_function_returned() {
    for program in ["values-go-o0", "values-go-o2"] {
        let mut dap = Dap::start(program);
        let (thread, _) = stop(
            &mut dap,
            program,
            &Configuration {
                functions: vec!["main.returning".to_owned()],
                ..Configuration::default()
            },
        );
        let sent = dap.send("stepOut", json!({"threadId": thread}));
        dap.success(sent);
        let stopped = dap.stopped(sent.mark);
        assert_eq!(stopped.reason, "step", "{program}");
        let trace = dap.request("stackTrace", json!({"threadId": thread, "levels": 1}));
        let variables = frame_variables(&mut dap, &trace["stackFrames"][0]);

        // Returned values are listed with the frame's own, and no name
        // reaches them.
        let count = &variables["returned count"];
        assert_eq!(count["value"], "42", "{program}: {variables:?}");
        assert!(count.get("evaluateName").is_none(), "{program}: {count}");
        assert_eq!(variables["returned text"]["value"], "\"go\"", "{program}");
        assert_eq!(variables["returned failure"]["value"], "nil", "{program}");
        finish_running(dap, thread);
    }
}

/// A C function's returned struct, from the registers System V returns
/// it in, has members to expand.
#[test]
fn step_out_shows_what_a_c_function_returned() {
    let program = "returns-c-gcc-o2";
    let mut dap = Dap::start(program);
    let (thread, _) = stop(
        &mut dap,
        program,
        &Configuration {
            functions: vec!["r_mixed".to_owned()],
            ..Configuration::default()
        },
    );
    let sent = dap.send("stepOut", json!({"threadId": thread}));
    dap.success(sent);
    assert_eq!(dap.stopped(sent.mark).reason, "step");
    let trace = dap.request("stackTrace", json!({"threadId": thread, "levels": 1}));
    let variables = frame_variables(&mut dap, &trace["stackFrames"][0]);
    let returned = &variables["returned r_mixed"];
    let reference = returned["variablesReference"]
        .as_i64()
        .expect("a struct has members");
    assert!(reference > 0, "{returned}");
    let members = dap.request("variables", json!({"variablesReference": reference}));
    let members = members["variables"].as_array().expect("members");
    let value = |name: &str| {
        members
            .iter()
            .find(|member| member["name"] == name)
            .map(|member| member["value"].clone())
    };

    assert_eq!(value("d"), Some(json!("0.5")), "{members:?}");
    assert_eq!(value("i"), Some(json!("42")), "{members:?}");
    finish_running(dap, thread);
}

/// The functions that left by tail calls stand between a frame and its
/// caller in a stack trace, and their scopes show what was passed to them.
#[test]
fn stack_traces_show_the_functions_that_left_by_tail_calls() {
    let program = "tail-frames-gcc-o2";
    let mut dap = Dap::start(program);
    let (thread, _) = stop(
        &mut dap,
        program,
        &Configuration {
            functions: vec!["leaf".to_owned()],
            ..Configuration::default()
        },
    );
    let trace = dap.request("stackTrace", json!({"threadId": thread, "levels": 4}));
    let frames = trace["stackFrames"].as_array().expect("frames");
    let line = |marker: &str| {
        i64::try_from(crate::support::source_line(
            "tests/fixtures/c/tail-frames.c",
            marker,
        ))
        .expect("a line fits")
    };

    assert_eq!(
        frames
            .iter()
            .map(|frame| (frame["name"].clone(), frame["line"].clone()))
            .skip(1)
            .collect::<Vec<_>>(),
        [
            (json!("middle [tail call]"), json!(line("frames: middle"))),
            (json!("top [tail call]"), json!(line("frames: top"))),
            (json!("main"), json!(line("frames: call top"))),
        ],
        "{frames:#?}"
    );
    let variables = frame_variables(&mut dap, &frames[1]);
    assert_eq!(variables["value"]["value"], "6", "{variables:?}");

    // Through `either`, which may have reached `leaf` two ways.
    let resumed = dap.send("continue", json!({"threadId": thread}));
    dap.success(resumed);
    dap.stopped(resumed.mark);
    let trace = dap.request("stackTrace", json!({"threadId": thread, "levels": 2}));
    assert_eq!(trace["stackFrames"][1]["name"], "main", "{trace:#?}");
    finish_running(dap, thread);
}

#[test]
fn python_frames_show_their_recorded_source_and_their_locals() {
    let mut dap = Dap::start("python");
    let started = dap.begin(
        Profile::VsCode,
        (
            "attach",
            json!({"coreFile": fixture("python-locals-3.14t.core")}),
        ),
        &Configuration::default(),
    );
    dap.stopped(started.mark);

    let threads = dap.request("threads", Value::Null);
    let frame = threads["threads"]
        .as_array()
        .expect("threads")
        .iter()
        .find_map(|thread| {
            let trace = dap.request(
                "stackTrace",
                json!({"threadId": thread["id"], "startFrame": 0, "levels": 64}),
            );
            trace["stackFrames"]
                .as_array()
                .expect("frames")
                .iter()
                .find(|frame| frame["name"] == "containers")
                .cloned()
        })
        .expect("the frame of containers");

    // An interpreted frame is where its interpreter recorded it, and runs
    // no machine code of its own.
    assert!(
        frame["source"]["path"]
            .as_str()
            .is_some_and(|path| path.ends_with("tests/fixtures/python/locals.py")),
        "{frame}"
    );
    // The checkpoint is the line before the function returns its locals.
    let returns = line_of(&source("python/locals.py"), "return numbers, empty_list");
    assert_eq!(frame["line"], returns - 1, "{frame}");
    assert!(
        frame.get("instructionPointerReference").is_none(),
        "{frame}"
    );

    let scopes = dap.request("scopes", json!({"frameId": frame["id"]}));
    let names: Vec<&str> = scopes["scopes"]
        .as_array()
        .expect("scopes")
        .iter()
        .filter_map(|scope| scope["name"].as_str())
        .collect();
    assert!(names.contains(&"Locals"), "{names:?}");
    assert!(!names.contains(&"Registers"), "{names:?}");

    let variables = frame_variables(&mut dap, &frame);
    assert_eq!(variables["numbers"]["value"], "[1, 2, 3]", "{variables:#?}");
    assert_eq!(variables["pair"]["value"], "(1, 'two')", "{variables:#?}");
    assert!(variables["numbers"]["variablesReference"].as_i64() > Some(0));

    // Expressions read the frame's objects in every context an editor
    // evaluates in, and a container expands as a local does.
    for context in ["watch", "hover", "repl"] {
        let evaluate = |dap: &mut Dap, expression: &str| {
            dap.request(
                "evaluate",
                json!({"expression": expression, "frameId": frame["id"], "context": context}),
            )
        };
        let last = evaluate(&mut dap, "numbers[-1]");
        assert_eq!(last["result"], "3", "{context}: {last}");
        let found = evaluate(&mut dap, "by_name[\"b\"]");
        assert_eq!(found["result"], "'two'", "{context}: {found}");
        let nested = evaluate(&mut dap, "nested[2]");
        assert_eq!(nested["result"], "{'k': [4]}", "{context}: {nested}");
        assert!(
            nested["variablesReference"].as_i64() > Some(0),
            "{context}: {nested}"
        );
    }
    dap.finish();
}
