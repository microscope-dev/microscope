//! Goroutines as a client sees them: as its threads.

use serde_json::{Value, json};

use crate::dap::{Configuration, Dap, Profile, fixture};

/// Launches the workers fixture to its checkpoint, where its workers are
/// parked, with `extra` launch arguments.
fn at_checkpoint(extra: Value) -> (Dap, i64) {
    let mut dap = Dap::start("goroutines");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("workers-go-o0"),
        extra,
        &Configuration {
            functions: vec!["main.reached".to_owned()],
            ..Configuration::default()
        },
    );
    let stop = dap.stopped(started.mark);
    assert_eq!(stop.reason, "function breakpoint", "{stop:?}");
    (dap, stop.thread)
}

fn threads(dap: &mut Dap) -> Vec<(i64, String)> {
    dap.request("threads", Value::Null)["threads"]
        .as_array()
        .expect("threads")
        .iter()
        .map(|thread| {
            (
                thread["id"].as_i64().expect("an id"),
                thread["name"].as_str().expect("a name").to_owned(),
            )
        })
        .collect()
}

#[test]
fn goroutines_are_threads_named_where_the_program_has_them() {
    let (mut dap, stopped) = at_checkpoint(json!({}));

    // The stop names main's goroutine, whose id is the client's.
    assert_eq!(stopped, 1);
    let listed = threads(&mut dap);
    let (first, name) = &listed[0];
    assert_eq!(*first, 1, "{listed:?}");
    assert!(
        name.starts_with("[1] main.reached — at breakpoint 1 (thread "),
        "{name}"
    );
    let workers = listed
        .iter()
        .filter(|(_, name)| name.contains("main.worker"))
        .collect::<Vec<_>>();
    assert_eq!(workers.len(), 4, "{listed:?}");
    for (id, name) in &workers {
        assert_eq!(*name, format!("[{id}] main.worker — chan receive"));
    }

    // The runtime's own goroutines are left out.
    assert!(
        !listed
            .iter()
            .any(|(_, name)| name.contains("runtime.forcegchelper")),
        "{listed:?}"
    );

    // A parked worker's stack is its own, with the runtime's frames
    // subdued, and its arguments are there.
    let worker = workers[0].0;
    let trace = dap.request("stackTrace", json!({"threadId": worker}));
    let frames = trace["stackFrames"].as_array().expect("frames");
    assert_eq!(frames[0]["name"], "runtime.gopark", "{trace}");
    assert_eq!(frames[0]["presentationHint"], "subtle", "{trace}");
    let frame = frames
        .iter()
        .find(|frame| frame["name"] == "main.worker")
        .unwrap_or_else(|| panic!("{trace}"));
    assert!(frame.get("presentationHint").is_none(), "{frame}");
    let jobs = dap.request(
        "evaluate",
        json!({"expression": "jobs", "frameId": frame["id"], "context": "watch"}),
    );
    assert!(
        jobs["result"]
            .as_str()
            .is_some_and(|result| !result.is_empty()),
        "{jobs}"
    );

    let task = dap.request(
        "evaluate",
        json!({"expression": "$task", "frameId": frame["id"], "context": "watch"}),
    );
    assert_eq!(task["result"], worker.to_string(), "{task}");
    dap.finish();
}

#[test]
fn launch_options_list_the_runtimes_goroutines_cut_the_list_or_list_threads() {
    let (mut dap, _) = at_checkpoint(json!({"runtimeTasks": true}));
    let all = threads(&mut dap);
    assert!(
        all.iter()
            .any(|(_, name)| name.contains("runtime.forcegchelper")),
        "{all:?}"
    );
    dap.finish();

    let (mut dap, _) = at_checkpoint(json!({"maxTasks": 3}));
    let cut = threads(&mut dap);
    assert_eq!(cut.len(), 4, "{cut:?}");
    assert_eq!(cut[0].0, 1, "{cut:?}");
    assert!(
        cut[3].1.starts_with("8 more goroutines not shown"),
        "{cut:?}"
    );
    let error = dap.request_error("stackTrace", json!({"threadId": cut[3].0}));
    assert!(error.contains("not a thread"), "{error}");
    dap.finish();

    let (mut dap, stopped) = at_checkpoint(json!({"threads": "system"}));
    let process = i64::from(dap.process_id().expect("a process"));

    // The main thread is the process's first.
    assert_eq!(stopped, process);
    let system = threads(&mut dap);
    assert!(system.iter().all(|(id, _)| *id >= process), "{system:?}");
    dap.finish();
}

/// A hundred thousand goroutines are cut to `maxTasks`, the stopped one
/// first, with the rest counted in a last entry.
#[test]
fn a_hundred_thousand_goroutines_are_cut_with_a_count_of_the_rest() {
    let mut dap = Dap::start("scale");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("scale-go"),
        json!({}),
        &Configuration {
            functions: vec!["main.checkpoint".to_owned()],
            ..Configuration::default()
        },
    );
    let stop = dap.stopped(started.mark);
    assert_eq!(stop.reason, "function breakpoint", "{stop:?}");
    let listed = threads(&mut dap);
    assert_eq!(listed.len(), 1001);
    assert!(
        listed[0]
            .1
            .starts_with("[1] main.checkpoint — at breakpoint 1 "),
        "{:?}",
        listed[0]
    );
    assert!(
        listed[1..1000]
            .iter()
            .all(|(_, name)| name.ends_with("main.park — chan receive")),
        "{:?}",
        &listed[..8]
    );

    // main's goroutine and every parked one.
    assert_eq!(
        listed[1000].1,
        "99001 more goroutines not shown; maxTasks lists 1000"
    );
    dap.finish();
}

/// A panic nothing recovers stops in the runtime, with the program's frame
/// that panicked selected. Clients focus the first frame whose source is
/// not deemphasized, so the runtime's frames above it are.
#[test]
fn an_unrecovered_panic_is_shown_where_the_program_panicked() {
    let mut dap = Dap::start("panic");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("panic-go-o0"),
        json!({}),
        &Configuration::default(),
    );
    let stop = dap.stopped(started.mark);
    assert_eq!(stop.reason, "exception", "{stop:?}");
    let trace = dap.request("stackTrace", json!({"threadId": stop.thread}));
    let frames = trace["stackFrames"].as_array().expect("frames");
    let focused = frames
        .iter()
        .find(|frame| {
            frame["source"].is_object() && frame["source"]["presentationHint"] != "deemphasize"
        })
        .unwrap_or_else(|| panic!("{trace}"));
    assert_eq!(focused["name"], "main.explode", "{trace}");

    // Below it, the program's callers are shown as they are.
    let caller = frames
        .iter()
        .find(|frame| frame["name"] == "main.main")
        .unwrap_or_else(|| panic!("{trace}"));
    assert!(
        caller["source"].get("presentationHint").is_none(),
        "{trace}"
    );
    dap.finish();
}

#[test]
fn a_stack_that_crosses_stacks_labels_each_run_of_frames() {
    let mut dap = Dap::start("stack labels");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("stacks-go-o0"),
        json!({}),
        &Configuration {
            functions: vec!["runtime.readmemstats_m".to_owned()],
            ..Configuration::default()
        },
    );
    let stop = dap.stopped(started.mark);

    // Main's goroutine stopped on the system stack.
    assert_eq!(stop.thread, 1, "{stop:?}");
    let trace = dap.request("stackTrace", json!({"threadId": 1}));
    let names = trace["stackFrames"]
        .as_array()
        .expect("frames")
        .iter()
        .map(|frame| {
            let name = frame["name"].as_str().expect("a name");
            if frame["presentationHint"] == "label" {
                format!("<{name}>")
            } else {
                name.to_owned()
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(
        names[..6],
        [
            "<on the runtime's stack>",
            "runtime.readmemstats_m",
            "runtime.ReadMemStats.func1",
            "runtime.systemstack",
            "<on the task's stack>",
            "runtime.ReadMemStats",
        ],
        "{trace}"
    );
    assert_eq!(trace["totalFrames"].as_u64(), Some(names.len() as u64));

    // Pages count the labels.
    let page = dap.request(
        "stackTrace",
        json!({"threadId": 1, "startFrame": 4, "levels": 2}),
    );
    assert_eq!(
        page["stackFrames"][0]["name"], "on the task's stack",
        "{page}"
    );
    assert_eq!(
        page["stackFrames"][1]["name"], "runtime.ReadMemStats",
        "{page}"
    );
    dap.finish();
}
