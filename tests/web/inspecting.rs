//! Reading a stop's values from the page: scopes, the value tree, watches,
//! changing values, and the console.

use serde_json::{Value, json};

use crate::support::Scenario;
use crate::web::{Client, Web};

fn fixture(name: &str) -> String {
    Scenario::fixture(name).display().to_string()
}

/// Runs kvstore to the line after `handle_request` looks its key up, and
/// returns that stop's frame.
async fn stop_in_handle_request(tab: &mut Client) -> Value {
    tab.state("the program loaded", |state| state["session"].is_string())
        .await;
    tab.ok("addBreakpoint", json!({"location": "kvstore.c:92"}))
        .await;
    tab.ok("continue", json!({})).await;
    let stopped = tab.inferior("stopped").await;
    let inferior = &stopped["inferior"];

    json!({"stop": inferior["stop"], "thread": inferior["thread"], "frame": 0})
}

/// The row named `name` among `rows`.
fn row<'a>(rows: &'a Value, name: &str) -> &'a Value {
    rows.as_array()
        .expect("rows")
        .iter()
        .find(|row| row["name"] == name)
        .unwrap_or_else(|| panic!("no {name} in {rows}"))
}

fn with(frame: &Value, extra: &Value) -> Value {
    let mut params = frame.clone();
    for (key, value) in extra.as_object().expect("an object") {
        params[key] = value.clone();
    }

    params
}

#[tokio::test]
async fn scopes_expand_into_children_that_belong_to_their_stop() {
    let web = Web::start("scopes", &[&fixture("kvstore")]);
    let mut tab = web.control("tab").await;
    let frame = stop_in_handle_request(&mut tab).await;

    let scopes = tab.ok("scopes", frame.clone()).await;
    let scopes = scopes["scopes"].as_array().expect("scopes").clone();
    let names = scopes.iter().map(|scope| &scope["key"]).collect::<Vec<_>>();
    assert_eq!(names, [&json!("args"), &json!("locals"), &json!("statics")]);
    let req = row(&scopes[0]["rows"], "req");
    assert_eq!(req["type"], "const request *");
    assert_eq!(req["path"], "req");
    let pointee = req["children"]["handle"].as_u64().expect("req expands");

    // A pointer expands to what it points to, named for the evaluator.
    let fields = tab
        .ok(
            "children",
            json!({"handle": pointee, "start": 0, "count": 100}),
        )
        .await;
    let key = row(&fields["rows"], "key");
    assert!(
        key["text"]
            .as_str()
            .is_some_and(|text| text.contains("user:")),
        "{key}"
    );
    assert_eq!(key["path"], "(*req).key");
    let len = row(&fields["rows"], "len");
    assert_eq!(len["editable"], true, "{len}");

    // A bounded page of an array's elements, which a later page continues.
    let table = tab
        .ok(
            "evaluate",
            with(&frame, &json!({"expression": "s->table.slots"})),
        )
        .await;
    assert_eq!(table["children"]["indexed"], 64, "{table}");
    let slots = table["children"]["handle"].as_u64().expect("slots expand");
    let page = tab
        .ok(
            "children",
            json!({"handle": slots, "start": 60, "count": 10}),
        )
        .await;

    let names = page["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| row["name"].as_str().unwrap_or_default())
        .collect::<Vec<_>>();
    assert_eq!(names, ["[60]", "[61]", "[62]", "[63]"]);

    // A handle belongs to its stop: once the program moves on it is stale.
    let thread = frame["thread"].clone();
    tab.ok(
        "step",
        json!({"stop": frame["stop"], "thread": thread, "kind": "over"}),
    )
    .await;
    tab.state("the next stop", |state| {
        state["inferior"]["state"] == "stopped" && state["inferior"]["stop"] != frame["stop"]
    })
    .await;
    let (kind, _) = tab
        .request(
            "children",
            json!({"handle": pointee, "start": 0, "count": 10}),
        )
        .await
        .expect_err("an earlier stop's handle");
    assert_eq!(kind, "staleStop");
    tab.save_traffic("values");
}

#[tokio::test]
async fn watches_read_values_and_only_changes_write_them() {
    let web = Web::start("watches", &[&fixture("kvstore")]);
    let mut tab = web.control("tab").await;
    let frame = stop_in_handle_request(&mut tab).await;

    let evaluate = |expression: &str| with(&frame, &json!({"expression": expression}));
    let first = tab.ok("evaluate", evaluate("req->key[0]")).await;
    assert_eq!(first["text"], "117 'u'", "{first}");

    // Watches and hovers never change the program.
    let (kind, message) = tab
        .request("evaluate", evaluate("s->stats.gets = 41"))
        .await
        .expect_err("an assignment in a watch");
    assert_eq!(kind, "invalid", "{message}");
    let (kind, _) = tab
        .request("evaluate", evaluate("s->"))
        .await
        .expect_err("a malformed expression");
    assert_eq!(kind, "invalid");

    // A change says so to every tab, whose values it makes out of date.
    let before = tab.latest_state().expect("a state")["writes"].clone();
    let written = tab
        .ok(
            "setValue",
            with(&frame, &json!({"path": "s->stats.gets", "value": "41"})),
        )
        .await;
    assert_eq!(written["text"], "41");
    tab.state("the write", |state| state["writes"] != before)
        .await;
    let read = tab.ok("evaluate", evaluate("s->stats.gets")).await;
    assert_eq!(read["text"], "41");
}

#[tokio::test]
async fn the_console_completes_and_runs_commands_in_the_tabs_frame() {
    let web = Web::start("console", &[&fixture("kvstore")]);
    let mut tab = web.control("tab").await;
    let frame = stop_in_handle_request(&mut tab).await;

    let members = tab
        .ok("complete", with(&frame, &json!({"text": "req->"})))
        .await;
    let labels = members["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["label"].as_str().unwrap_or_default())
        .collect::<Vec<_>>();

    for member in ["op", "key", "value", "len"] {
        assert!(labels.contains(&member), "{members}");
    }

    assert_eq!(members["start"], 5);
    let commands = tab.ok("complete", json!({"text": "inf"})).await;
    assert_eq!(commands["items"][0]["label"], "info", "{commands}");

    // A frame's names read as themselves; a command reads in the frame.
    let value = tab
        .ok("console", with(&frame, &json!({"line": "req->len"})))
        .await;
    assert!(value["row"]["text"].is_string(), "{value}");
    let printed = tab
        .ok(
            "console",
            with(&frame, &json!({"line": "frame", "frame": 1})),
        )
        .await;
    assert!(
        printed["output"]
            .as_str()
            .is_some_and(|output| output.contains("worker")),
        "{printed}"
    );
    let (kind, message) = tab
        .request("console", with(&frame, &json!({"line": "continue"})))
        .await
        .expect_err("run control from the console");
    assert_eq!(kind, "invalid", "{message}");

    // A viewer reads values and evaluates, but neither writes nor runs commands.
    let link = tab.ok("share", json!({"role": "view", "to": "/"})).await["url"]
        .as_str()
        .expect("a link")
        .to_owned();
    let mut viewer = web.joining("viewer", &link).await;
    viewer.ok("scopes", frame.clone()).await;
    viewer
        .ok("console", with(&frame, &json!({"line": "req->len"})))
        .await;

    for (method, params) in [
        ("console", with(&frame, &json!({"line": "frame"}))),
        ("console", with(&frame, &json!({"line": "req->len = 2"}))),
        (
            "setValue",
            with(&frame, &json!({"path": "req->len", "value": "2"})),
        ),
    ] {
        let (kind, _) = viewer
            .request(method, params)
            .await
            .expect_err("a viewer's change");
        assert_eq!(kind, "forbidden", "{method}");
    }
}

#[tokio::test]
async fn functions_are_found_by_any_part_of_their_name() {
    let web = Web::start("functions", &[&fixture("kvstore")]);
    let mut tab = web.control("tab").await;
    tab.state("the program loaded", |state| state["session"].is_string())
        .await;
    let names = |found: &Value| {
        found["functions"]
            .as_array()
            .expect("functions")
            .iter()
            .map(|function| function["name"].as_str().unwrap_or_default().to_owned())
            .collect::<Vec<_>>()
    };

    // Before the program runs, where each is declared.
    let found = tab.ok("functions", json!({"query": "handle_req"})).await;
    assert_eq!(found["functions"][0]["name"], "handle_request", "{found}");
    assert_eq!(found["functions"][0]["line"], 89);
    assert!(
        found["functions"][0]["path"]
            .as_str()
            .is_some_and(|path| path.ends_with("kvstore.c")),
        "{found}"
    );

    // Letters in order find a name, after names that hold them together.
    let scattered = tab.ok("functions", json!({"query": "tfind"})).await;
    assert!(
        names(&scattered).contains(&"table_find".to_owned()),
        "{scattered}"
    );
    let entry = tab.ok("functions", json!({"query": "entry"})).await;
    assert_eq!(names(&entry)[0], "entry_set", "{entry}");

    let few = tab.ok("functions", json!({"query": "e", "limit": 2})).await;
    assert_eq!(names(&few).len(), 2);
    assert_eq!(few["more"], true);
    let none = tab.ok("functions", json!({"query": "  "})).await;
    assert!(names(&none).is_empty());
}

/// After a step out, the locals of the frame returned to include what the
/// function returned, which expands but which no path names.
#[tokio::test]
async fn a_step_out_lists_what_the_function_returned() {
    let web = Web::start("returned", &[&fixture("returns-c-gcc-o2")]);
    let mut tab = web.control("tab").await;
    tab.state("the program loaded", |state| state["session"].is_string())
        .await;
    tab.ok("addBreakpoint", json!({"location": "r_mixed"}))
        .await;
    tab.ok("continue", json!({})).await;
    let stopped = tab.inferior("stopped").await;
    let (stop, thread) = (
        stopped["inferior"]["stop"].clone(),
        stopped["inferior"]["thread"].clone(),
    );

    tab.ok(
        "step",
        json!({"stop": stop, "thread": thread, "kind": "out"}),
    )
    .await;

    let out = tab
        .state("the step out", |state| {
            state["inferior"]["state"] == "stopped" && state["inferior"]["stop"] != stop
        })
        .await;

    let frame = json!({"stop": out["inferior"]["stop"], "thread": thread, "frame": 0});
    let scopes = tab.ok("scopes", frame).await;
    let locals = scopes["scopes"]
        .as_array()
        .expect("scopes")
        .iter()
        .find(|scope| scope["key"] == "locals")
        .expect("locals")
        .clone();

    let returned = row(&locals["rows"], "returned r_mixed");
    assert!(returned["path"].is_null(), "{returned}");
    let members = returned["children"]["handle"]
        .as_u64()
        .expect("the struct expands");

    let fields = tab
        .ok(
            "children",
            json!({"handle": members, "start": 0, "count": 10}),
        )
        .await;
    assert_eq!(row(&fields["rows"], "i")["text"], "42", "{fields}");
    assert_eq!(row(&fields["rows"], "d")["text"], "0.5", "{fields}");
}

/// A backtrace shows the functions that left by tail calls, and selecting
/// one shows what was passed to it.
#[tokio::test]
async fn the_stack_shows_functions_that_left_by_tail_calls() {
    let web = Web::start("tail", &[&fixture("tail-frames-gcc-o2")]);
    let mut tab = web.control("tab").await;
    tab.state("the program loaded", |state| state["session"].is_string())
        .await;
    tab.ok("addBreakpoint", json!({"location": "leaf"})).await;
    tab.ok("continue", json!({})).await;
    let stopped = tab.inferior("stopped").await;
    let (stop, thread) = (
        stopped["inferior"]["stop"].clone(),
        stopped["inferior"]["thread"].clone(),
    );
    let trace = tab
        .ok("backtrace", json!({"stop": stop, "thread": thread}))
        .await;
    let frames = trace["frames"].as_array().expect("frames");
    assert_eq!(
        frames[..4]
            .iter()
            .map(|frame| (frame["name"].clone(), frame["kind"].clone()))
            .collect::<Vec<_>>(),
        [
            (json!("leaf"), json!("physical")),
            (json!("middle"), json!("tailCall")),
            (json!("top"), json!("tailCall")),
            (json!("main"), json!("physical")),
        ],
        "{trace}"
    );

    let scopes = tab
        .ok(
            "scopes",
            json!({"stop": stop, "thread": thread, "frame": frames[1]["index"]}),
        )
        .await;
    let arguments = scopes["scopes"]
        .as_array()
        .expect("scopes")
        .iter()
        .find(|scope| scope["key"] == "args")
        .expect("arguments")
        .clone();
    assert_eq!(row(&arguments["rows"], "value")["text"], "6", "{arguments}");
}

/// A Python program's stack marks each frame with its language, places
/// its Python frames where the interpreter recorded them, and folds the
/// interpreter's frames between them.
#[tokio::test]
async fn a_python_stack_marks_its_languages_and_folds_the_interpreter() {
    let core = Scenario::fixture("python-locals-3.14t.core");
    let core = core.to_str().expect("a UTF-8 path");
    let web = Web::start("python", &["--core", core]);
    let mut tab = web.control("tab").await;
    let state = tab
        .state("the core opened", |state| {
            state["inferior"]["stop"].is_u64()
        })
        .await;
    let stop = state["inferior"]["stop"].clone();
    let mut found = None;

    for thread in state["threads"].as_array().expect("threads") {
        let trace = tab
            .ok("backtrace", json!({"stop": stop, "thread": thread["id"]}))
            .await;
        let frames = trace["frames"].as_array().expect("frames");
        if let Some(index) = frames
            .iter()
            .position(|frame| frame["name"] == "containers")
        {
            found = Some((trace.clone(), index, thread["id"].clone()));
        }
    }

    let (trace, index, thread) = found.expect("the frame of containers");
    let frames = trace["frames"].as_array().expect("frames");
    let frame = &frames[index];
    assert_eq!(frame["kind"], "interpreted", "{frame}");
    assert_eq!(frame["language"], json!({"mark": "py", "name": "Python"}));
    assert!(frame["address"].is_null(), "{frame}");
    assert!(
        frame["source"]["path"]
            .as_str()
            .is_some_and(|path| path.ends_with("tests/fixtures/python/locals.py")),
        "{frame}"
    );

    // Each fold is two or more of the interpreter's frames, none of them
    // Python's.
    let folds = trace["folds"].as_array().expect("folds");
    assert!(!folds.is_empty(), "{trace}");
    for fold in folds {
        let first = usize::try_from(fold["first"].as_u64().expect("first")).expect("an index");
        let count = usize::try_from(fold["count"].as_u64().expect("count")).expect("a count");
        assert!(count >= 2, "{fold}");
        for folded in &frames[first..first + count] {
            assert_ne!(folded["kind"], "interpreted", "{trace}");
            assert!(folded["address"].is_string(), "{folded}");
        }
    }

    // Expressions read the frame's objects, and a container expands.
    let at = json!({"stop": stop, "thread": thread, "frame": frame["index"]});
    let evaluate = |expression: &str| with(&at, &json!({"expression": expression}));
    let last = tab.ok("evaluate", evaluate("numbers[-1]")).await;
    assert_eq!(last["text"], "3", "{last}");
    let found = tab.ok("evaluate", evaluate("by_name[\"b\"]")).await;
    assert_eq!(found["text"], "'two'", "{found}");
    let nested = tab.ok("evaluate", evaluate("nested[2]")).await;
    assert_eq!(nested["text"], "{'k': [4]}", "{nested}");
    assert!(nested["children"]["handle"].is_u64(), "{nested}");
}
