//! A profile's system call trace, as the Syscalls view reads it: what each
//! call cost, when, from where, and what it did with files and processes.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use crate::profiles::listed;
use crate::support::{Scenario, ScratchDir, flight_recordings};
use crate::web::{Client, Web};

/// The syscalls fixture's trace in `mode`, by the ptrace engine with
/// stacks, written in `directory`, where the program runs.
fn trace(directory: &Path, mode: &str) -> PathBuf {
    let path = directory.join(format!("{mode}.usprof"));
    let mut command = Command::new(env!("CARGO_BIN_EXE_microscope"));
    command
        .args(["syscalls", "--engine", "ptrace", "-f", "-k", "-o"])
        .arg(&path)
        .arg("--")
        .arg(Scenario::fixture("syscalls"))
        .arg(mode)
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null());

    // A failing test keeps the tracer's recording beside the server's.
    let recording = flight_recordings::watch_adapter(&mut command);
    let status = command.status().expect("run microscope syscalls");
    if let Some(recording) = &recording {
        flight_recordings::adapter_finished(recording);
    }

    assert!(status.code().is_some(), "{status}");
    path
}

/// The tab, and the id of the one profile its server serves.
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

/// A line's text, its tokens joined.
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

/// Every view of one trace agrees with what the program did: it opened,
/// wrote, read and unlinked one file, failed to open another, and exited
/// with 3.
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

    // The failed open, as strace prints it with -y, and expanded.
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

    // The failure is placed at the line that made it.
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

/// The failed open, expanded: its arguments, stack, and source line.
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

/// The files the program used, and every selected call charted once.
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

    // Every selected call lands in the charts once.
    let selected = json!({"profile": id, "filter": "trace=%file", "buckets": 16});
    let rate = tab.ok("syscallRate", selected.clone()).await;
    let charted: u64 = rate["series"]
        .as_array()
        .expect("series")
        .iter()
        .map(|series| sum(&series["counts"]))
        .sum();
    let of_files = tab
        .ok(
            "syscallSummary",
            json!({"profile": id, "filter": "trace=%file"}),
        )
        .await;
    assert_eq!(Some(charted), of_files["calls"].as_u64(), "{rate}");
    let heat = tab.ok("syscallHeatmap", selected.clone()).await;
    assert_eq!(heat["counts"].as_array().expect("counts").len(), 16 * 40);
    assert_eq!(Some(sum(&heat["counts"])), of_files["calls"].as_u64());
    let busy = tab.ok("syscallLanes", selected).await;
    assert_eq!(busy["lanes"].as_array().expect("lanes").len(), 1, "{busy}");
    assert!(sum(&busy["lanes"][0]["busy"]) > 0);
}

/// Calls grouped by what they name and where they were made, and a heat
/// map's row selecting the calls it counts.
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

    // Each row of the heat map holds the calls of its latency band.
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

/// Processes come parents first, each with its threads, calls, and end;
/// each thread has a lane.
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
    let lanes = tab
        .ok("syscallLanes", json!({"profile": id, "buckets": 8}))
        .await;
    assert_eq!(
        lanes["lanes"].as_array().expect("lanes").len(),
        3,
        "{lanes}"
    );

    // The parent is blocked across the middle of its wait for the child.
    let waits = tab
        .ok(
            "syscallLog",
            json!({"profile": id, "filter": "trace=wait4", "offset": 0, "limit": 10}),
        )
        .await;

    // Exit lines show whatever the selection, as strace prints them.
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
    let lanes = tab
        .ok(
            "syscallLanes",
            json!({"profile": id, "range": middle, "buckets": 4}),
        )
        .await;
    let waiting = lanes["lanes"]
        .as_array()
        .expect("lanes")
        .iter()
        .find(|lane| lane["tid"] == wait["tid"])
        .unwrap_or_else(|| panic!("{lanes}"));
    assert_eq!(
        waiting["blocked"],
        json!([true, true, true, true]),
        "{waiting}"
    );
}

/// A trace recorded from the page is published as it grows, says what its
/// program is blocked in, and ends when stopped.
#[tokio::test]
async fn a_live_trace_shows_what_its_program_is_blocked_in() {
    let web = Web::start("syscalls-live", &[]);
    let mut tab = web.control("tab").await;
    let program = Scenario::fixture("syscalls");
    let refused = tab
        .request(
            "recordProfile",
            json!({"program": program, "syscalls": {"filter": "trace=nosuchcall"}}),
        )
        .await;
    assert!(refused.is_err(), "{refused:?}");
    let started = tab
        .ok(
            "recordProfile",
            json!({"program": program, "arguments": ["pause"], "syscalls": {"stacks": true}}),
        )
        .await;
    let id = started["profile"].as_str().expect("an id").to_owned();

    // Each revision is read until the pause shows in flight.
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
