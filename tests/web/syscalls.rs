use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use crate::profiles::{listed, tree};
use crate::support::{Scenario, ScratchDir, flight_recordings};
use crate::web::{Client, Web};

fn trace(directory: &Path, mode: &str) -> PathBuf {
    let path = directory.join(format!("{mode}.usprof"));
    let mut command = Command::new(env!("CARGO_BIN_EXE_microscope"));
    command
        .args(["record", "--allow-stopping", "-o"])
        .arg(&path)
        .arg("--")
        .arg(Scenario::fixture("syscalls"))
        .arg(mode)
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null());

    let recording = flight_recordings::watch_adapter(&mut command);
    let status = command.status().expect("run microscope record");
    if let Some(recording) = &recording {
        flight_recordings::adapter_finished(recording);
    }

    assert!(status.code().is_some(), "{status}");
    path
}

async fn serve(web: &Web) -> (Client, String) {
    let mut tab = web.control("tab").await;
    let listed = tab
        .expect("the profiles", |message| message["type"] == "profiles")
        .await;
    let id = listed["profiles"][0]["id"]
        .as_str()
        .expect("an id")
        .to_owned();
    (tab, id)
}

fn text(line: &Value) -> String {
    line["tokens"]
        .as_array()
        .expect("tokens")
        .iter()
        .map(|token| token["text"].as_str().expect("text"))
        .collect()
}

fn sum(values: &Value) -> u64 {
    values
        .as_array()
        .expect("numbers")
        .iter()
        .map(|value| value.as_u64().expect("a count"))
        .sum()
}

#[tokio::test]
async fn a_trace_is_served_as_costs_lines_callers_files_and_lanes() {
    let scratch = ScratchDir::new("web-syscalls");
    let path = trace(scratch.path(), "files");
    let web = Web::start("syscalls", &[path.to_str().expect("a path")]);
    let (mut tab, id) = serve(&web).await;

    let summary = tab
        .ok(
            "syscallSummary",
            json!({"profile": id, "filter": "trace=write,read,unlink"}),
        )
        .await;
    let rows = summary["rows"].as_array().expect("rows");
    let calls = |name: &str| {
        rows.iter()
            .find(|row| row["name"] == name)
            .map(|row| row["calls"].as_u64().expect("calls"))
    };

    assert_eq!(
        (calls("write"), calls("unlink")),
        (Some(1), Some(1)),
        "{summary}"
    );
    assert_eq!(summary["engine"], "ptrace");
    assert!(summary["total"].as_u64() > summary["calls"].as_u64());

    let log = tab
        .ok("syscallLog", json!({"profile": id, "filter": "trace=openat status=failed", "offset": 0, "limit": 100}))
        .await;
    let lines = log["lines"].as_array().expect("lines");
    let missing = lines
        .iter()
        .find(|line| text(line).contains("\"missing\""))
        .unwrap_or_else(|| panic!("{log}"));
    assert_eq!(
        text(missing),
        format!(
            "openat(AT_FDCWD<{}>, \"missing\", O_RDONLY) = -1 ENOENT (No such file or directory)",
            scratch.path().display()
        )
    );
    assert_eq!(missing["failed"], true);
    expanded(&mut tab, &id, &missing["event"]).await;

    let detail = tab
        .ok("syscallDetail", json!({"profile": id, "call": "openat"}))
        .await;
    assert_eq!(detail["errnos"][0]["name"], "ENOENT", "{detail}");
    let callers = detail["callers"].as_array().expect("callers");
    assert!(
        callers
            .iter()
            .any(|caller| caller["site"] == "files · syscalls.c:24" && caller["failed"] == 1),
        "{detail}"
    );
    assert_eq!(sum(&detail["whole"]), sum(&detail["selected"]));

    under_a_function(&mut tab, &id).await;
    files_and_charts(&mut tab, &id, scratch.path()).await;
    groups_and_bands(&mut tab, &id, scratch.path()).await;

    let processes = tab.ok("syscallProcesses", json!({"profile": id})).await;
    assert_eq!(
        processes["processes"][0]["end"], "exited with 3",
        "{processes}"
    );
    let network = tab.ok("syscallNetwork", json!({"profile": id})).await;
    assert_eq!(network["endpoints"], json!([]));

    let refused = tab
        .request(
            "syscallSummary",
            json!({"profile": id, "filter": "trace=nosuchcall"}),
        )
        .await;
    assert!(refused.is_err(), "{refused:?}");
    tab.save_traffic("syscalls");
}

async fn expanded(tab: &mut Client, id: &str, event: &Value) {
    let call = tab
        .ok("syscallCall", json!({"profile": id, "event": event}))
        .await;
    let arguments: Vec<&str> = call["arguments"]
        .as_array()
        .expect("arguments")
        .iter()
        .map(|argument| argument["name"].as_str().expect("a name"))
        .collect();
    assert_eq!(arguments[..3], ["dfd", "filename", "flags"], "{call}");
    let stack: Vec<&str> = call["stack"]
        .as_array()
        .expect("a stack")
        .iter()
        .map(|name| name.as_str().expect("a name"))
        .collect();
    assert!(
        stack.contains(&"files") && stack.contains(&"main"),
        "{call}"
    );

    let site = &call["site"];
    assert_eq!(
        (&site["function"], &site["line"]),
        (&json!("files"), &json!(24))
    );

    let first = site["first"].as_u64().expect("a first line");
    let made = &site["text"][usize::try_from(24 - first).expect("an index")];
    assert!(
        made.as_str()
            .is_some_and(|line| line.contains("\"missing\"")),
        "{site}"
    );
}

async fn under_a_function(tab: &mut Client, id: &str) {
    let focused = json!({"profile": id, "transforms": [{"op": "focus", "name": "files"}]});
    let summary = tab.ok("syscallSummary", focused.clone()).await;
    let (total, calls) = (summary["total"].as_u64(), summary["calls"].as_u64());
    assert!(calls > Some(0) && calls < total, "{summary}");

    let mut sites = focused;
    sites["group"] = json!("site");
    let sites = tab.ok("syscallSummary", sites).await;
    let names: Vec<&str> = sites["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .filter_map(|row| row["name"].as_str())
        .collect();
    assert!(
        !names.is_empty() && names.iter().all(|name| name.starts_with("files ·")),
        "{sites}"
    );

    let dropped = json!({"profile": id, "transforms": [{"op": "drop", "name": "files"}]});
    let dropped = tab.ok("syscallSummary", dropped).await;
    assert_eq!(
        dropped["calls"]
            .as_u64()
            .zip(calls)
            .map(|(left, right)| left + right),
        total,
        "{dropped}"
    );

    let unknown = json!({"profile": id, "transforms": [{"op": "focus", "name": "nosuchfunction"}]});
    let refused = tab.request("syscallSummary", unknown).await;
    assert!(
        format!("{refused:?}").contains("nosuchfunction"),
        "{refused:?}"
    );
}

async fn files_and_charts(tab: &mut Client, id: &str, scratch: &Path) {
    let present = scratch.join("present").display().to_string();
    let files = tab.ok("syscallFiles", json!({"profile": id})).await;
    let file = |path: &str| {
        files["files"]
            .as_array()
            .expect("files")
            .iter()
            .find(|file| file["path"] == path)
            .cloned()
            .unwrap_or_else(|| panic!("{path}: {files}"))
    };

    let written = file(&present);
    assert_eq!(
        (&written["opens"], &written["written"], &written["read"]),
        (&json!(2), &json!(6), &json!(6)),
        "{written}"
    );
    let failed = file(&scratch.join("missing").display().to_string());
    assert_eq!(
        (&failed["opens"], &failed["failed"]),
        (&json!(1), &json!(1))
    );

    let selected = json!({"profile": id, "filter": "trace=%file", "buckets": 16});
    let of_files = tab
        .ok(
            "syscallSummary",
            json!({"profile": id, "filter": "trace=%file"}),
        )
        .await;
    let heat = tab.ok("syscallHeatmap", selected).await;
    assert_eq!(heat["counts"].as_array().expect("counts").len(), 16 * 40);
    assert_eq!(Some(sum(&heat["counts"])), of_files["calls"].as_u64());
    assert_eq!(Some(sum(&heat["failed"])), of_files["errors"].as_u64());
    assert!(
        heat["failed"]
            .as_array()
            .expect("failed")
            .iter()
            .zip(heat["counts"].as_array().expect("counts"))
            .all(|(failed, count)| failed.as_u64() <= count.as_u64()),
        "{heat}"
    );

    let all = tab.ok("syscallSummary", json!({"profile": id})).await;
    let timeline = tab
        .ok("profileTimeline", json!({"profile": id, "buckets": 16}))
        .await;
    let lanes = timeline["lanes"].as_array().expect("lanes");
    let counted: u64 = lanes.iter().map(|lane| sum(&lane["syscalls"])).sum();
    assert_eq!(Some(counted), all["calls"].as_u64(), "{timeline}");
    let failed: u64 = lanes.iter().map(|lane| sum(&lane["failed"])).sum();
    assert_eq!(Some(failed), all["errors"].as_u64(), "{timeline}");
}

async fn groups_and_bands(tab: &mut Client, id: &str, scratch: &Path) {
    let grouped = |group: &str| json!({"profile": id, "filter": "trace=openat", "group": group});
    let by_path = tab.ok("syscallSummary", grouped("path")).await;
    let missing = scratch.join("missing").display().to_string();
    let row = by_path["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|row| row["key"] == missing.as_str())
        .unwrap_or_else(|| panic!("{by_path}"));
    assert_eq!((&row["calls"], &row["errors"]), (&json!(1), &json!(1)));
    let by_site = tab.ok("syscallSummary", grouped("site")).await;
    assert!(
        by_site["rows"]
            .as_array()
            .expect("rows")
            .iter()
            .any(|row| row["key"] == "files · syscalls.c:24" && row["errors"] == 1),
        "{by_site}"
    );

    let heat = tab
        .ok(
            "syscallHeatmap",
            json!({"profile": id, "filter": "trace=openat", "buckets": 1}),
        )
        .await;
    let (band, count) = heat["counts"]
        .as_array()
        .expect("counts")
        .iter()
        .enumerate()
        .find(|(_, count)| count.as_u64() > Some(0))
        .expect("a counted band");
    let banded = tab
        .ok(
            "syscallSummary",
            json!({"profile": id, "filter": "trace=openat", "latency": band}),
        )
        .await;
    assert_eq!(&banded["calls"], count, "{banded}");
}

#[tokio::test]
async fn a_familys_processes_and_threads_are_served_in_order() {
    let scratch = ScratchDir::new("web-syscalls-family");
    let path = trace(scratch.path(), "family");
    let web = Web::start("syscalls", &[path.to_str().expect("a path")]);
    let (mut tab, id) = serve(&web).await;
    let processes = tab.ok("syscallProcesses", json!({"profile": id})).await;
    let processes = processes["processes"].as_array().expect("processes");
    assert_eq!(processes.len(), 2, "{processes:?}");
    let (parent, child) = (&processes[0], &processes[1]);
    assert_eq!(child["parent"], parent["pid"]);
    assert_eq!(child["end"], "exited with 4");
    assert_eq!(parent["threads"], 2, "{parent}");
    let timeline = tab
        .ok("profileTimeline", json!({"profile": id, "buckets": 8}))
        .await;
    let called = timeline["lanes"]
        .as_array()
        .expect("lanes")
        .iter()
        .filter(|lane| sum(&lane["syscalls"]) > 0)
        .count();
    assert_eq!(called, 3, "{timeline}");

    let waits = tab
        .ok(
            "syscallLog",
            json!({"profile": id, "filter": "trace=wait4", "offset": 0, "limit": 10}),
        )
        .await;

    let wait = waits["lines"]
        .as_array()
        .expect("lines")
        .iter()
        .find(|line| line["call"] == true)
        .unwrap_or_else(|| panic!("{waits}"));
    let (ended, took) = (
        wait["time"].as_i64().expect("a time"),
        wait["durationNs"].as_i64().expect("a duration"),
    );
    let middle = [ended - took * 3 / 4, ended - took / 4];
    let summary = tab.ok("profile", json!({"profile": id})).await;
    let row = summary["threads"]
        .as_array()
        .expect("threads")
        .iter()
        .find(|thread| thread["tid"] == wait["tid"])
        .map_or_else(|| panic!("{summary}"), |thread| thread["row"].clone());
    let timeline = tab
        .ok(
            "profileTimeline",
            json!({"profile": id, "range": middle, "buckets": 4}),
        )
        .await;
    let waiting = timeline["lanes"]
        .as_array()
        .expect("lanes")
        .iter()
        .find(|lane| lane["thread"] == row)
        .unwrap_or_else(|| panic!("{timeline}"));
    assert_eq!(
        waiting["waits"],
        json!([{"call": "wait4", "start": middle[0], "end": middle[1]}]),
        "{waiting}"
    );
}

#[tokio::test]
async fn a_live_trace_shows_what_its_program_is_blocked_in() {
    let web = Web::start("syscalls-live", &[]);
    let mut tab = web.control("tab").await;
    let program = Scenario::fixture("syscalls");
    let refused = tab
        .request(
            "recordProfile",
            json!({"program": program, "preset": "production", "allowStopping": true}),
        )
        .await;
    assert!(
        format!("{refused:?}").contains("never stops the program"),
        "{refused:?}"
    );
    let started = tab
        .ok(
            "recordProfile",
            json!({
                "program": program,
                "arguments": ["pause"],
                "syscalls": "calls",
                "allowStopping": true,
            }),
        )
        .await;
    let id = started["profile"].as_str().expect("an id").to_owned();

    let mut seen = -1;

    let blocked = loop {
        let info = listed(&mut tab, &id, "a newer revision", |info| {
            info["revision"].as_i64() > Some(seen)
        })
        .await;
        assert_eq!(info["recording"], true, "{info}");
        seen = info["revision"].as_i64().expect("a revision");
        let flight = tab
            .ok("syscallInFlight", json!({"profile": id, "revision": seen}))
            .await;
        assert_eq!(flight["live"], true);
        if let Some(call) = flight["calls"]
            .as_array()
            .expect("calls")
            .iter()
            .find(|call| text(call).starts_with("pause("))
        {
            break call.clone();
        }
    };

    assert!(
        blocked["blockedNs"].as_u64() >= Some(1_000_000),
        "{blocked}"
    );
    let summary = tab
        .ok("syscallSummary", json!({"profile": id, "revision": seen}))
        .await;
    assert_eq!(summary["engine"], "ptrace");
    assert!(summary["calls"].as_u64() > Some(0), "{summary}");

    tab.ok("stopRecording", json!({"profile": id})).await;
    let ended = listed(&mut tab, &id, "the trace's end", |info| {
        info["recording"] == false
    })
    .await;
    assert_eq!(ended["ended"], "killed by SIGKILL", "{ended}");

    let flight = tab.ok("syscallInFlight", json!({"profile": id})).await;
    assert_eq!(flight, json!({"live": false, "calls": []}));
    crate::web::wait_gone(blocked["pid"].as_u64().expect("a pid"));
}

#[tokio::test]
async fn samples_and_calls_are_selected_through_each_other() {
    let scratch = ScratchDir::new("web-syscalls-work");
    let path = trace(scratch.path(), "work");
    let web = Web::start("syscalls-work", &[path.to_str().expect("a path")]);
    let (mut tab, id) = serve(&web).await;

    let plain = tree(&mut tab, json!({"profile": id})).await;
    assert!(
        plain
            .iter()
            .all(|(path, _, _)| path.iter().all(|name| !name.starts_with("[syscall]"))),
        "{plain:?}"
    );

    let leaves = tree(&mut tab, json!({"profile": id, "syscallLeaves": true})).await;
    let ending = |nodes: &[(Vec<String>, u64, u64)], name: &str| -> Vec<Vec<String>> {
        nodes
            .iter()
            .filter(|(path, own, _)| *own > 0 && path.last().is_some_and(|last| last == name))
            .map(|(path, _, _)| path.clone())
            .collect()
    };
    let slept: u64 = leaves
        .iter()
        .filter(|(path, _, _)| {
            path.last()
                .is_some_and(|last| last == "[syscall] clock_nanosleep")
        })
        .map(|(_, own, _)| own)
        .sum();
    assert!(slept >= 25 * 2_000_000 * 9 / 10, "slept {slept} ns");
    let rests = ending(&leaves, "[syscall] clock_nanosleep");
    let draws = ending(&leaves, "[syscall] getrandom");
    assert!(
        rests
            .iter()
            .all(|path| path.iter().any(|name| name == "rest")),
        "{rests:?}"
    );
    assert!(
        draws
            .iter()
            .any(|path| path.iter().any(|name| name == "draw")),
        "{draws:?}"
    );

    let sites = tab
        .ok(
            "syscallSummary",
            json!({"profile": id, "filter": "trace=clock_nanosleep", "group": "site"}),
        )
        .await;
    let site = sites["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|row| {
            row["name"]
                .as_str()
                .is_some_and(|name| name.starts_with("rest"))
        })
        .and_then(|row| row["key"].as_str())
        .unwrap_or_else(|| panic!("{sites}"))
        .to_owned();
    let during = |key: &str| {
        json!({
            "profile": id,
            "syscallLeaves": true,
            "during": {"filter": "", "row": {"group": "site", "key": key}},
        })
    };
    let rested = tree(&mut tab, during(&site)).await;
    let weighed: u64 = rested.iter().map(|(_, own, _)| own).sum();
    assert_eq!(weighed, slept, "{rested:?}");
    assert!(
        rested
            .iter()
            .filter(|(_, own, _)| *own > 0)
            .all(|(path, _, _)| path.iter().any(|name| name == "rest")),
        "{rested:?}"
    );
    let nowhere = tree(&mut tab, during("nowhere · nosuch.c:1")).await;
    assert!(nowhere.iter().all(|(_, own, _)| *own == 0), "{nowhere:?}");

    let refused = tab
        .request(
            "profileTree",
            json!({"profile": id, "during": {"filter": "trace=nosuchcall"}}),
        )
        .await;
    assert!(format!("{refused:?}").contains("nosuchcall"), "{refused:?}");
}
