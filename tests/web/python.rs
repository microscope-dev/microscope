//! A Python program debugged from the page: its breakpoints wait for its
//! code, each stop the agent makes opens on the program's frame, not the
//! agent's, and the page can read the source that frame runs.

use serde_json::{Value, json};

use crate::support::python;
use crate::web::{Client, Web};

/// The one-based line of `uat.py` that ends with `marker`.
fn line(marker: &str) -> usize {
    let text = std::fs::read_to_string(python::fixture("uat.py")).expect("read the fixture");
    text.lines()
        .position(|line| line.ends_with(marker))
        .expect("a marked line")
        + 1
}

/// Checks that a stop selects the program's frame `function` at `line`,
/// says so in its place and history, and folds every frame inside it.
async fn opens_on(tab: &mut Client, state: &Value, function: &str, line: usize) -> Value {
    let inferior = &state["inferior"];
    let frame = inferior["frame"].as_u64().expect("the stop's frame");
    assert!(
        frame > 0,
        "the agent's frames are inside the program's: {state}"
    );
    assert_eq!(inferior["place"]["function"], function, "{state}");
    assert_eq!(inferior["place"]["line"], line, "{state}");
    let entry = state["stops"].as_array().and_then(|stops| stops.last());
    assert_eq!(
        entry.map(|entry| &entry["frame"]),
        Some(&json!(frame)),
        "{state}"
    );

    let (stop, thread) = (inferior["stop"].clone(), inferior["thread"].clone());
    let trace = tab
        .ok("backtrace", json!({"stop": stop, "thread": thread}))
        .await;
    let index = usize::try_from(frame).expect("an index");
    let selected = &trace["frames"][index];
    assert_eq!(selected["name"], function, "{trace}");
    assert_eq!(selected["kind"], "interpreted", "{trace}");
    // The interpreter holds the frame, which no machine state describes:
    // that is how such a frame is, not a failure.
    let (kind, message) = tab
        .request(
            "registers",
            json!({"stop": stop, "thread": thread, "frame": frame}),
        )
        .await
        .expect_err("an interpreted frame's registers");
    assert_eq!(kind, "unsupported", "{message}");
    let fold = &trace["folds"][0];
    assert_eq!(
        (&fold["first"], &fold["count"]),
        (&json!(0), &json!(frame)),
        "the frames inside the program's fold: {trace}"
    );
    stop
}

#[tokio::test]
async fn a_python_breakpoint_waits_for_its_code_and_stops_open_on_the_program() {
    let interpreter = python::interpreter("3.14");
    let script = python::fixture("uat.py");
    let web = Web::start(
        "python",
        &[
            interpreter.to_str().expect("a UTF-8 path"),
            "--",
            script.to_str().expect("a UTF-8 path"),
        ],
    );
    let mut tab = web.control("tab").await;
    tab.state("the program loaded", |state| state["session"].is_string())
        .await;
    // A bare name is a native function's, and none is `area`, so the
    // refusal says how to name Python's.
    let (kind, message) = tab
        .request("addBreakpoint", json!({"location": "area"}))
        .await
        .expect_err("no native function is named area");
    assert_eq!(kind, "invalid");
    assert!(message.contains("`py:area`"), "{message}");

    let area = line("# AREA_LINE");
    tab.ok(
        "addBreakpoint",
        json!({"location": format!("uat.py:{area}")}),
    )
    .await;

    tab.ok("continue", json!({})).await;
    let hit = tab.inferior("stopped").await;
    assert_eq!(hit["inferior"]["reason"]["kind"], "breakpoint", "{hit}");
    let stop = opens_on(&mut tab, &hit, "area", area).await;

    // The page shows the source the program's frame runs, which no debug
    // information names, but reads nothing else.
    let path = hit["inferior"]["place"]["path"].clone();
    let source = tab.ok("source", json!({"path": path})).await;
    assert!(
        source["text"]
            .as_str()
            .is_some_and(|text| text.contains("def area(side):")),
        "{source}"
    );
    let (kind, _) = tab
        .request("source", json!({"path": "/etc/passwd"}))
        .await
        .expect_err("a file nothing names");
    assert_eq!(kind, "invalid");

    let thread = hit["inferior"]["thread"].clone();
    tab.ok(
        "step",
        json!({"stop": stop, "thread": thread, "kind": "over"}),
    )
    .await;
    let stepped = tab
        .state("the step", |state| {
            state["inferior"]["state"] == "stopped" && state["inferior"]["stop"] != stop
        })
        .await;
    assert_eq!(stepped["inferior"]["reason"]["kind"], "step", "{stepped}");
    let stop = opens_on(&mut tab, &stepped, "area", area + 1).await;

    // Python's exceptions stop only once chosen, which the console's
    // `catch` sees too, and the stop opens on the frame that raised.
    let filters = tab.ok("exceptions", json!(null)).await;
    let raised = filters["filters"]
        .as_array()
        .and_then(|filters| {
            filters
                .iter()
                .find(|filter| filter["id"] == "python-raised")
        })
        .cloned()
        .unwrap_or_else(|| panic!("python-raised: {filters}"));
    assert_eq!(raised["on"], false, "{raised}");
    assert_eq!(raised["language"], "Python", "{raised}");
    let (kind, _) = tab
        .request(
            "setException",
            json!({"id": "python-everything", "on": true}),
        )
        .await
        .expect_err("a filter no runtime has");
    assert_eq!(kind, "invalid");
    let settings = stepped["settings"].clone();
    tab.ok("setException", json!({"id": "python-raised", "on": true}))
        .await;
    tab.state("the setting", |state| state["settings"] != settings)
        .await;
    let catch = tab
        .ok("console", json!({"line": "catch python-raised"}))
        .await;
    assert!(
        catch["output"]
            .as_str()
            .is_some_and(|output| output.contains("python-raised") && output.contains(" on ")),
        "{catch}"
    );

    tab.ok("continue", json!({"stop": stop})).await;
    let raising = tab
        .state("the exception", |state| {
            state["inferior"]["state"] == "stopped" && state["inferior"]["stop"] != stop
        })
        .await;
    assert_eq!(
        raising["inferior"]["reason"]["kind"], "languageException",
        "{raising}"
    );
    opens_on(&mut tab, &raising, "fail", line("# RAISE_LINE")).await;
}

/// Each Python frame says where its function begins, and the page can read
/// the source of every frame it was shown, a caller's in another file too.
#[tokio::test]
async fn python_frames_give_their_functions_first_lines_and_sources() {
    let script = python::fixture("page.py");
    let text = std::fs::read_to_string(&script).expect("read the fixture");
    let marked = text
        .lines()
        .position(|line| line.ends_with("# STOCK_LINE"))
        .expect("a marked line")
        + 1;
    // A decorated function begins at its first decorator.
    let first = text
        .lines()
        .position(|line| line.starts_with("@contextmanager"))
        .expect("the function")
        + 1;
    let interpreter = python::interpreter("3.14");
    let web = Web::start(
        "python-frames",
        &[
            interpreter.to_str().expect("a UTF-8 path"),
            "--",
            script.to_str().expect("a UTF-8 path"),
        ],
    );
    let mut tab = web.control("tab").await;
    tab.state("the program loaded", |state| state["session"].is_string())
        .await;
    tab.ok(
        "addBreakpoint",
        json!({"location": format!("page.py:{marked}")}),
    )
    .await;
    tab.ok("continue", json!({})).await;
    let hit = tab.inferior("stopped").await;
    let inferior = &hit["inferior"];
    let trace = tab
        .ok(
            "backtrace",
            json!({"stop": inferior["stop"], "thread": inferior["thread"]}),
        )
        .await;
    let frames = trace["frames"].as_array().expect("frames");
    let selected =
        &frames[usize::try_from(inferior["frame"].as_u64().expect("a frame")).expect("an index")];
    assert_eq!(selected["source"]["line"], marked, "{selected}");
    assert_eq!(selected["source"]["first"], first, "{selected}");

    let caller = frames
        .iter()
        .find(|frame| {
            frame["source"]["path"]
                .as_str()
                .is_some_and(|path| path.ends_with("/contextlib.py"))
        })
        .unwrap_or_else(|| panic!("contextlib's frame: {trace}"));
    let source = tab
        .ok("source", json!({"path": caller["source"]["path"]}))
        .await;
    assert!(
        source["text"]
            .as_str()
            .is_some_and(|text| text.contains("def contextmanager(")),
        "{source}"
    );
}

/// A coroutine an asyncio task runs is stepped by the interpreter's
/// machinery, `_asyncio`'s included, which folds between it and the event
/// loop's Python frames as it does between any two Python frames.
#[tokio::test]
async fn a_tasks_frames_fold_the_machinery_that_steps_it() {
    let script = python::fixture("stepping.py");
    let text = std::fs::read_to_string(&script).expect("read the fixture");
    let marked = text
        .lines()
        .position(|line| line.ends_with("# TASK_END"))
        .expect("a marked line")
        + 1;
    let interpreter = python::interpreter("3.14");
    let web = Web::start(
        "python-task",
        &[
            interpreter.to_str().expect("a UTF-8 path"),
            "--",
            script.to_str().expect("a UTF-8 path"),
        ],
    );
    let mut tab = web.control("tab").await;
    tab.state("the program loaded", |state| state["session"].is_string())
        .await;
    tab.ok(
        "addBreakpoint",
        json!({"location": format!("stepping.py:{marked}")}),
    )
    .await;
    tab.ok("continue", json!({})).await;
    let hit = tab.inferior("stopped").await;
    let inferior = &hit["inferior"];
    let trace = tab
        .ok(
            "backtrace",
            json!({"stop": inferior["stop"], "thread": inferior["thread"]}),
        )
        .await;
    let frames = trace["frames"].as_array().expect("frames");
    let task = usize::try_from(inferior["frame"].as_u64().expect("a frame")).expect("an index");
    assert_eq!(frames[task]["name"], "task", "{trace}");
    let caller = (task + 1..frames.len())
        .find(|&index| frames[index]["kind"] == "interpreted")
        .unwrap_or_else(|| panic!("the event loop's frames: {trace}"));
    let folded = |index: usize| {
        trace["folds"].as_array().is_some_and(|folds| {
            folds.iter().any(|fold| {
                let first = fold["first"].as_u64().unwrap_or(u64::MAX);
                let count = fold["count"].as_u64().unwrap_or(0);
                (first..first + count).contains(&(index as u64))
            })
        })
    };
    let shown = (task + 1..caller)
        .filter(|&index| !folded(index))
        .map(|index| frames[index]["name"].clone())
        .collect::<Vec<_>>();
    assert!(shown.is_empty(), "machinery shown: {shown:?} in {trace}");
}

/// Before a Python program runs, the page lists and reads the Python files
/// of the directory Python imports the script's modules from, opens on the
/// script, and still reads nothing else.
#[tokio::test]
async fn a_python_programs_own_files_are_listed_and_read_before_it_runs() {
    let script = python::fixture("uat.py");
    let directory = script.parent().expect("the fixtures' directory");
    let interpreter = python::interpreter("3.14");
    let web = Web::start(
        "python-files",
        &[
            interpreter.to_str().expect("a UTF-8 path"),
            "--",
            script.to_str().expect("a UTF-8 path"),
        ],
    );
    let mut tab = web.control("tab").await;
    tab.state("the program loaded", |state| state["session"].is_string())
        .await;

    let sources = tab.ok("sources", json!(null)).await;
    let files = sources["files"]
        .as_array()
        .expect("files")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    let named = |name: &str| directory.join(name).display().to_string();
    for listed in [
        named("uat.py"),
        named("page.py"),
        named("site/sitecustomize.py"),
    ] {
        assert!(files.contains(&listed.as_str()), "{listed} in {sources}");
    }
    assert!(
        files.iter().all(|file| !file.contains("__pycache__")),
        "{sources}"
    );
    assert_eq!(sources["entry"]["path"], named("uat.py"), "{sources}");
    assert_eq!(sources["entry"]["line"], 1, "{sources}");

    let page = tab.ok("source", json!({"path": named("page.py")})).await;
    assert!(
        page["text"]
            .as_str()
            .is_some_and(|text| text.contains("def stocked(")),
        "{page}"
    );
    for refused in [
        "/etc/passwd".to_owned(),
        named("../../../src/runtime_model/python/agent.py"),
    ] {
        let (kind, _) = tab
            .request("source", json!({"path": refused}))
            .await
            .expect_err("a file outside the program's own");
        assert_eq!(kind, "invalid", "{refused}");
    }
}

/// A breakpoint set in the file the page opens before a Python program runs,
/// at the path it opened, is in that file once the program stops there: the
/// stop, the breakpoint, and the file the page opened name one path.
#[tokio::test]
async fn a_breakpoint_set_in_the_opened_script_is_in_it_at_the_stop() {
    let interpreter = python::interpreter("3.14");
    let web = Web::start(
        "python-gutter",
        &[
            interpreter.to_str().expect("a UTF-8 path"),
            "--",
            "./tests/fixtures/python/uat.py",
        ],
    );
    let mut tab = web.control("tab").await;
    tab.state("the program loaded", |state| state["session"].is_string())
        .await;
    let sources = tab.ok("sources", json!(null)).await;
    let opened = sources["entry"]["path"].clone();
    let area = line("# AREA_LINE");
    let location = format!("{}:{area}", opened.as_str().expect("a path"));
    tab.ok("addBreakpoint", json!({"location": location})).await;

    tab.ok("continue", json!({})).await;
    let hit = tab.inferior("stopped").await;
    assert_eq!(hit["inferior"]["place"]["path"], opened, "{hit}");
    let breakpoint = &hit["breakpoints"][0];
    let places = breakpoint["places"].as_array().expect("places");
    assert!(
        places
            .iter()
            .any(|place| place["path"] == opened && place["line"] == area),
        "{breakpoint}"
    );
}
