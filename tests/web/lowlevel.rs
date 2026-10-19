//! The program below its source: disassembly, memory, registers,
//! watchpoints, signals, and modules, as the page reads them.

use serde_json::{Value, json};

use crate::support::Scenario;
use crate::web::{Client, Web};

fn fixture(name: &str) -> String {
    Scenario::fixture(name).display().to_string()
}

/// Runs kvstore to the line after `handle_request` looks its key up.
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

fn frame(at: &Value, frame: u32) -> Value {
    let mut at = at.clone();
    at["frame"] = frame.into();
    at
}

fn instructions(disassembled: &Value) -> &Vec<Value> {
    disassembled["instructions"]
        .as_array()
        .expect("instructions")
}

#[tokio::test]
async fn disassembly_shows_each_frames_function_and_where_it_is() {
    let web = Web::start("disassembly", &[&fixture("kvstore")]);
    let mut tab = web.control("tab").await;
    let at = stop_in_handle_request(&mut tab).await;
    let trace = tab
        .ok(
            "backtrace",
            json!({"stop": at["stop"], "thread": at["thread"]}),
        )
        .await;

    let inner = tab.ok("disassemble", at.clone()).await;
    assert_eq!(inner["function"], "handle_request", "{inner}");
    assert_eq!(inner["marked"], trace["frames"][0]["address"]);
    let marked = instructions(&inner)
        .iter()
        .find(|instruction| instruction["address"] == inner["marked"])
        .expect("the program counter's instruction");
    assert!(!marked["tokens"].as_array().expect("tokens").is_empty());

    // A call names where it goes, so the page can follow it.
    let call = instructions(&inner)
        .iter()
        .find(|instruction| {
            instruction["target"]["name"]
                .as_str()
                .is_some_and(|name| name.starts_with("table_find"))
        })
        .expect("the call to table_find");
    assert_eq!(call["tokens"][0]["kind"], "mnemonic");
    assert!(
        instructions(&inner)
            .iter()
            .any(|instruction| instruction["source"]["line"] == 92),
        "{inner}"
    );

    // A caller's mark is its call, which the return address follows.
    let outer = tab.ok("disassemble", frame(&at, 1)).await;
    assert_eq!(outer["function"], "worker");
    let called = instructions(&outer)
        .iter()
        .find(|instruction| instruction["address"] == outer["marked"])
        .expect("the caller's call");
    assert!(
        called["target"]["name"]
            .as_str()
            .is_some_and(|name| name.starts_with("handle_request")),
        "{called}"
    );

    // Any address shows the function that holds it.
    let address = call["target"]["address"].clone();
    let elsewhere = tab
        .ok(
            "disassemble",
            json!({"stop": at["stop"], "thread": at["thread"], "frame": 0, "address": address}),
        )
        .await;
    assert_eq!(elsewhere["function"], "table_find");
    assert_eq!(elsewhere["marked"], Value::Null);

    // AT&T syntax, for those who read it.
    let mut att = at.clone();
    att["syntax"] = "att".into();
    let att = tab.ok("disassemble", att).await;
    assert!(
        instructions(&att)
            .iter()
            .any(|instruction| instruction["tokens"].to_string().contains('%')),
        "{att}"
    );

    let registers = tab.ok("registers", at.clone()).await;
    let rip = registers["registers"]
        .as_array()
        .expect("registers")
        .iter()
        .find(|register| register["name"] == "rip")
        .expect("rip");
    assert_eq!(rip["role"], "pc");

    let pc = u64::from_str_radix(
        trace["frames"][0]["address"]
            .as_str()
            .expect("an address")
            .trim_start_matches("0x"),
        16,
    )
    .expect("hexadecimal");
    assert_eq!(rip["value"], format!("{pc:#018x}"));
    tab.save_traffic("lowlevel");
}

#[tokio::test]
async fn memory_reads_and_writes_at_the_stop_it_names() {
    let web = Web::start("memory", &[&fixture("kvstore")]);
    let mut tab = web.control("tab").await;
    let at = stop_in_handle_request(&mut tab).await;
    let mut key = at.clone();
    key["expression"] = "req->key".into();
    let key = tab.ok("evaluate", key).await;
    let address = key["memory"].clone();

    // The array is stored in the request, sixteen bytes of it.
    assert_eq!(key["memoryBytes"], 16, "{key}");

    // A pointer's memory is its pointee, whose size it does not know.
    let mut req = at.clone();
    req["expression"] = "req".into();
    assert_eq!(tab.ok("evaluate", req).await["memoryBytes"], Value::Null);

    let read = tab
        .ok(
            "readMemory",
            json!({"stop": at["stop"], "address": address, "count": 5}),
        )
        .await;

    // "user:"
    assert_eq!(read["bytes"], "757365723a", "{read}");
    assert_eq!(read["unreadable"], Value::Null);

    // An unmapped page reads as far as it can, and says where it stopped.
    let unmapped = tab
        .ok(
            "readMemory",
            json!({"stop": at["stop"], "address": "0x10", "count": 4}),
        )
        .await;
    assert_eq!(unmapped["bytes"], "");
    assert_eq!(unmapped["unreadable"], "0x10");

    let before = tab.latest_state().expect("a state")["writes"].clone();
    tab.ok(
        "writeMemory",
        json!({"stop": at["stop"], "address": address, "bytes": "55"}),
    )
    .await;
    tab.state("the write", |state| state["writes"] != before)
        .await;

    let mut first = at.clone();
    first["expression"] = "req->key[0]".into();
    assert_eq!(tab.ok("evaluate", first).await["text"], "85 'U'");

    // Bytes belong to a stop: a passed one reads nothing.
    tab.ok(
        "step",
        json!({"stop": at["stop"], "thread": at["thread"], "kind": "over"}),
    )
    .await;
    tab.state("the next stop", |state| {
        state["inferior"]["state"] == "stopped" && state["inferior"]["stop"] != at["stop"]
    })
    .await;
    let (kind, _) = tab
        .request(
            "readMemory",
            json!({"stop": at["stop"], "address": address, "count": 5}),
        )
        .await
        .expect_err("a passed stop's memory");
    assert_eq!(kind, "staleStop");
}

#[tokio::test]
async fn watchpoints_follow_expressions_and_addresses() {
    let web = Web::start("watch", &[&fixture("kvstore")]);
    let mut tab = web.control("tab").await;
    let at = stop_in_handle_request(&mut tab).await;

    let mut watch = at.clone();
    watch["target"] = "s->stats.puts".into();
    watch["access"] = "change".into();
    tab.ok("addWatchpoint", watch).await;
    let watched = tab
        .state("the watchpoint", |state| {
            state["watchpoints"][0]["expression"] == "s->stats.puts"
        })
        .await;
    let id = watched["watchpoints"][0]["id"].clone();
    assert_eq!(watched["watchpoints"][0]["access"], "change");
    tab.ok(
        "editWatchpoint",
        json!({"id": id, "condition": "s->stats.puts > 0"}),
    )
    .await;
    tab.state("the condition", |state| {
        state["watchpoints"][0]["condition"] == "s->stats.puts > 0"
    })
    .await;
    let (kind, _) = tab
        .request(
            "addWatchpoint",
            json!({"target": "s->stats.gets", "access": "read"}),
        )
        .await
        .expect_err("an expression with no frame");
    assert_eq!(kind, "invalid");
    tab.ok("removeWatchpoint", json!({"id": id})).await;

    // Bytes at an address are watched without a frame.
    let mut gets = at.clone();
    gets["expression"] = "s->stats.gets".into();
    let gets = tab.ok("evaluate", gets).await;
    assert_eq!(gets["memoryBytes"], 8, "{gets}");
    let address = gets["memory"].clone();

    // Either case of prefix names an address.
    let target = format!(
        "{}:8",
        address
            .as_str()
            .expect("an address")
            .replacen("0x", "0X", 1)
    );
    tab.ok(
        "addWatchpoint",
        json!({"target": target, "access": "write"}),
    )
    .await;
    let watched = tab
        .state("the address watchpoint", |state| {
            state["watchpoints"][0]["address"] == address
        })
        .await;
    assert_eq!(watched["watchpoints"][0]["bytes"], 8);
    let id = watched["watchpoints"][0]["id"].clone();
    tab.ok("removeWatchpoint", json!({"id": id})).await;
    tab.state("no watchpoints", |state| {
        state["watchpoints"].as_array().is_some_and(Vec::is_empty)
    })
    .await;

    // A viewer reads all of it but changes none of it.
    let link = tab.ok("share", json!({"role": "view", "to": "/"})).await["url"]
        .as_str()
        .expect("a link")
        .to_owned();
    let mut viewer = web.joining("viewer", &link).await;
    viewer.ok("modules", json!(null)).await;
    viewer.ok("registers", at.clone()).await;

    for (method, params) in [
        (
            "writeMemory",
            json!({"stop": at["stop"], "address": "0x1000", "bytes": "00"}),
        ),
        (
            "addWatchpoint",
            json!({"target": "0x1000:4", "access": "write"}),
        ),
        (
            "setSignal",
            json!({"signal": 10, "stop": true, "print": true, "pass": true}),
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
async fn signals_and_modules_are_part_of_the_session() {
    let web = Web::start("signals", &[&fixture("kvstore")]);
    let mut tab = web.control("tab").await;
    stop_in_handle_request(&mut tab).await;

    let signals = tab.ok("signals", json!(null)).await;
    let usr1 = signals["signals"]
        .as_array()
        .expect("signals")
        .iter()
        .find(|signal| signal["name"] == "SIGUSR1")
        .expect("SIGUSR1")
        .clone();
    assert_eq!(usr1["stop"], true);
    let before = tab.latest_state().expect("a state")["settings"].clone();
    tab.ok(
        "setSignal",
        json!({"signal": usr1["signal"], "stop": false, "print": true, "pass": true}),
    )
    .await;
    tab.state("the new policy", |state| state["settings"] != before)
        .await;

    let changed = tab.ok("signals", json!(null)).await;
    assert!(
        changed["signals"]
            .as_array()
            .expect("signals")
            .iter()
            .any(|signal| signal["name"] == "SIGUSR1" && signal["stop"] == false),
        "{changed}"
    );

    let modules = tab.ok("modules", json!(null)).await;
    let modules = modules["modules"].as_array().expect("modules");
    assert_eq!(modules[0]["name"], "kvstore");
    assert_eq!(modules[0]["symbols"], "debug");
    assert!(
        modules.iter().any(|module| module["name"]
            .as_str()
            .is_some_and(|name| name.starts_with("libc"))),
        "{modules:?}"
    );
}

/// A module stripped of its debug information names the separate file it
/// came from, found in the server's debug directories.
#[tokio::test]
async fn modules_name_their_separate_debug_files() {
    let root = fixture("split/debug-root");
    let web = Web::start(
        "debug files",
        &["--debug-directory", &root, &fixture("split/basic-build-id")],
    );
    let mut tab = web.control("tab").await;
    tab.state("the program loaded", |state| state["session"].is_string())
        .await;
    tab.ok("addBreakpoint", json!({"location": "breakpoint_target"}))
        .await;
    tab.ok("continue", json!({})).await;
    tab.inferior("stopped").await;

    let modules = tab.ok("modules", json!(null)).await;
    let program = &modules["modules"][0];
    assert_eq!(program["name"], "basic-build-id", "{modules}");
    assert_eq!(program["symbols"], "debug");
    assert!(
        program["debugFile"]
            .as_str()
            .is_some_and(|path| path.starts_with(&root)
                && std::path::Path::new(path)
                    .extension()
                    .is_some_and(|extension| extension == "debug")),
        "{program}"
    );
    assert_eq!(program["debugFileProblem"], Value::Null);
}
