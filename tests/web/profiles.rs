//! Profiles a server serves, whatever it debugs: what one holds, its call
//! tree's columns, its functions, and its timeline, as the page reads them.

use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use microscope::profiler::{RecordOptions, SampleSource, Watch, record, record_watched};
use serde_json::{Value, json};

use crate::support::{Scenario, ScratchDir};
use crate::web::{Client, Web};

/// A profile of each of the six calls of `leaf` in the stacks fixture:
/// three on the main thread and three on its worker.
fn record_calls(scratch: &ScratchDir) -> std::path::PathBuf {
    record_calls_of(scratch, "leaf", "calls.usprof")
}

/// A profile of each call of `symbol` in the stacks fixture, in `name`.
fn record_calls_of(scratch: &ScratchDir, symbol: &str, name: &str) -> std::path::PathBuf {
    let mut command = Command::new(Scenario::fixture("profile-stacks-gcc-o0"));
    command.arg("1").stdout(Stdio::null());
    let options = RecordOptions {
        source: SampleSource::Function {
            symbol: symbol.into(),
        },
        function_period: 1,
        ..RecordOptions::default()
    };

    let recording = record(command, &options).expect("record");
    let path = scratch.path().join(name);
    recording.profile.write(&path).expect("write the profile");
    path
}

/// A diff's nodes: each one's path of names from the root, and its own
/// weight now and in the base.
fn diff_nodes(tree: &Value, bytes: &[u8]) -> Vec<(Vec<String>, u64, u64)> {
    let count = usize::try_from(tree["nodes"].as_u64().expect("a count")).expect("a count");
    assert_eq!(bytes.len(), count * 40, "six columns of {count} nodes");
    let mut nodes = nodes(tree, &bytes[..count * 24]);
    let base_own = bytes[count * 24..][..count * 8].as_chunks::<8>().0;

    for (node, word) in nodes.iter_mut().zip(base_own) {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "weights are whole"
        )]
        let weight = f64::from_le_bytes(*word) as u64;
        node.2 = weight;
    }

    nodes
}

/// A tree or functions answer with a base holds both profiles' weights,
/// against another profile or a profile's own other self; a base in
/// another profile cannot keep this one's thread filter.
#[tokio::test]
async fn a_profile_diffs_against_another_and_against_itself() {
    let scratch = ScratchDir::new("web-diff");
    let leaf = record_calls(&scratch);
    let middle = record_calls_of(&scratch, "middle", "middle.usprof");
    let web = Web::start(
        "profile",
        &[
            leaf.to_str().expect("a path"),
            middle.to_str().expect("a path"),
        ],
    );
    let mut tab = web.control("tab").await;
    let listed = tab
        .expect("both profiles", |message| {
            message["type"] == "profiles" && message["profiles"].as_array().map(Vec::len) == Some(2)
        })
        .await;
    let id = |name: &str| {
        listed["profiles"]
            .as_array()
            .expect("profiles")
            .iter()
            .find(|profile| profile["name"] == name)
            .and_then(|profile| profile["id"].as_str())
            .expect("an id")
            .to_owned()
    };

    let (leaf, middle) = (id("calls.usprof"), id("middle.usprof"));

    // Against itself, nothing changed anywhere.
    let (answer, bytes) = tab
        .with_bytes(
            "profileTree",
            json!({"profile": leaf, "base": {"profile": leaf}}),
        )
        .await
        .expect("a diff");
    assert_eq!(answer["base"]["weight"], 6);
    let same = diff_nodes(&answer, &bytes);
    assert!(same.iter().all(|(_, own, base)| own == base), "{same:?}");

    // Against the calls of middle, each profile's weight is on its paths.
    let (answer, bytes) = tab
        .with_bytes(
            "profileTree",
            json!({"profile": leaf, "base": {"profile": middle}}),
        )
        .await
        .expect("a diff");
    let diff = diff_nodes(&answer, &bytes);
    let base_weight = answer["base"]["weight"].as_u64().expect("a weight");
    assert!(base_weight > 0);
    assert_eq!(diff.iter().map(|(_, own, _)| own).sum::<u64>(), 6);
    assert_eq!(
        diff.iter().map(|(_, _, base)| base).sum::<u64>(),
        base_weight
    );
    for (path, own, base) in &diff {
        let last = path.last().map(String::as_str);
        assert!(*own == 0 || last == Some("leaf"), "{path:?}");
        assert!(*base == 0 || last == Some("middle"), "{path:?}");
    }

    assert!(diff.iter().any(|(path, own, base)| *own == 0
        && *base > 0
        && path.last().is_some_and(|name| name == "middle")));

    let functions = tab
        .ok(
            "profileFunctions",
            json!({"profile": leaf, "base": {"profile": middle}}),
        )
        .await;
    let row = |name: &str| {
        functions["rows"]
            .as_array()
            .expect("rows")
            .iter()
            .find(|row| {
                let key = usize::try_from(row["key"].as_u64().expect("a key")).expect("a key");
                functions["keys"][key]["name"] == name
            })
            .cloned()
            .unwrap_or_else(|| panic!("{name} in {functions}"))
    };

    assert_eq!(
        (&row("leaf")["own"], &row("leaf")["baseOwn"]),
        (&json!(6), &json!(0))
    );
    assert_eq!(row("middle")["baseOwn"], base_weight);

    focus_and_refusal(&mut tab, &leaf, &middle).await;
}

/// A focus on what the base never ran leaves it nothing, not an error; a
/// base in another profile cannot keep this one's thread filter.
async fn focus_and_refusal(tab: &mut Client, leaf: &str, middle: &str) {
    let (answer, _) = tab
        .with_bytes(
            "profileTree",
            json!({
                "profile": leaf,
                "transforms": [{"op": "focus", "name": "leaf"}],
                "base": {"profile": middle},
            }),
        )
        .await
        .expect("a focused diff");
    assert_eq!(
        (&answer["weight"], &answer["base"]["weight"]),
        (&json!(6), &json!(0))
    );

    let refused = tab
        .request(
            "profileTree",
            json!({"profile": leaf, "threads": [0], "base": {"profile": middle}}),
        )
        .await
        .expect_err("a thread filter of another profile");
    assert!(refused.1.contains("thread or task filter"), "{refused:?}");
}

/// A call tree's nodes, from its answer and the columns before it: each
/// node's path of names from the root, own weight, and total weight.
fn nodes(tree: &Value, bytes: &[u8]) -> Vec<(Vec<String>, u64, u64)> {
    let count = usize::try_from(tree["nodes"].as_u64().expect("a count")).expect("a count");
    assert_eq!(bytes.len(), count * 24, "four columns of {count} nodes");
    let f64s = |column: usize| {
        bytes[column * count * 8..][..count * 8]
            .as_chunks::<8>()
            .0
            .iter()
            .map(|word| f64::from_le_bytes(*word))
            .collect::<Vec<_>>()
    };

    let u32s = |offset: usize| {
        bytes[offset..][..count * 4]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|word| u32::from_le_bytes(*word) as usize)
            .collect::<Vec<_>>()
    };

    let (own, total) = (f64s(0), f64s(1));
    let (keys, depths) = (u32s(count * 16), u32s(count * 20));
    let mut path: Vec<String> = Vec::new();
    (0..count)
        .map(|node| {
            path.truncate(depths[node]);
            path.push(
                tree["keys"][keys[node]]["name"]
                    .as_str()
                    .expect("a name")
                    .to_owned(),
            );
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "weights are whole"
            )]
            let weight = |weight: f64| weight as u64;
            (path.clone(), weight(own[node]), weight(total[node]))
        })
        .collect()
}

async fn tree(tab: &mut Client, query: Value) -> Vec<(Vec<String>, u64, u64)> {
    let (answer, bytes) = tab
        .with_bytes("profileTree", query)
        .await
        .unwrap_or_else(|error| panic!("a tree: {error:?}"));
    nodes(&answer, &bytes)
}

/// A directory of a continuous recording's segments is served as one
/// profile holding every segment's samples.
#[tokio::test]
async fn a_directory_of_segments_is_served_as_one_profile() {
    let scratch = ScratchDir::new("web-segments");
    let calls = microscope::profiler::Profile::read(&record_calls(&scratch))
        .expect("read the profile")
        .profile;
    let directory = scratch.path().join("segments");
    std::fs::create_dir(&directory).expect("a directory");
    for _ in 0..2 {
        calls
            .write_segment(&directory, Duration::from_hours(1))
            .expect("write a segment");
    }

    let web = Web::start("profile", &[directory.to_str().expect("a path")]);
    let mut tab = web.control("tab").await;
    let listed = tab
        .expect("the profiles", |message| message["type"] == "profiles")
        .await;
    assert_eq!(listed["profiles"][0]["name"], "segments");
    let id = listed["profiles"][0]["id"].as_str().expect("an id");
    let summary = tab.ok("profile", json!({"profile": id})).await;
    assert_eq!(summary["samples"], 12, "{summary}");
    assert_eq!(
        summary["threads"].as_array().map(Vec::len),
        Some(2),
        "{summary}"
    );
}

#[tokio::test]
async fn a_profile_is_served_as_trees_functions_and_a_timeline() {
    let scratch = ScratchDir::new("web-profile");
    let path = record_calls(&scratch);
    let web = Web::start("profile", &[path.to_str().expect("a path")]);
    let mut tab = web.control("tab").await;

    // The server serves the profile, and its link opens it.
    let listed = tab
        .expect("the profiles", |message| message["type"] == "profiles")
        .await;
    let id = listed["profiles"][0]["id"]
        .as_str()
        .expect("an id")
        .to_owned();
    assert_eq!(listed["profiles"][0]["name"], "calls.usprof");
    assert_eq!(
        web.link_to.as_deref(),
        Some(format!("/profile/{id}").as_str())
    );

    let summary = tab.ok("profile", json!({"profile": id})).await;
    assert_eq!(summary["samples"], 6);
    assert_eq!(summary["unit"], "executions");
    let threads = summary["threads"].as_array().expect("threads");
    assert_eq!(threads.len(), 2, "{summary}");
    assert!(threads.iter().all(|thread| thread["samples"] == 3));

    // Every call ends in leaf, under main's descent or the worker's.
    let everything = tree(&mut tab, json!({"profile": id})).await;
    let leaves: Vec<_> = everything.iter().filter(|(_, own, _)| *own > 0).collect();
    assert!(
        leaves
            .iter()
            .all(|(path, _, _)| path.last().is_some_and(|name| name == "leaf"))
    );
    assert_eq!(leaves.iter().map(|(_, own, _)| own).sum::<u64>(), 6);
    let roots: u64 = everything
        .iter()
        .filter(|(path, _, _)| path.len() == 1)
        .map(|(_, _, total)| total)
        .sum();
    assert_eq!(roots, 6);

    // Focused on middle and inverted: middle's callers, from it outward.
    let callers = tree(
        &mut tab,
        json!({"profile": id, "invert": true, "transforms": [{"op": "focus", "name": "middle"}]}),
    )
    .await;
    assert_eq!(callers[0], (vec!["middle".to_owned()], 0, 6));
    assert_eq!(callers[1].0, ["middle", "outer"]);

    // One thread's samples only.
    let worker = threads
        .iter()
        .find(|thread| thread["tid"] != thread["pid"])
        .expect("the worker")["row"]
        .clone();
    let one_thread = tree(&mut tab, json!({"profile": id, "threads": [worker]})).await;
    assert!(
        one_thread
            .iter()
            .any(|(path, _, _)| path.iter().any(|name| name == "worker"))
    );
    assert!(
        one_thread
            .iter()
            .all(|(path, _, _)| !path.iter().any(|name| name == "main"))
    );

    let functions = tab.ok("profileFunctions", json!({"profile": id})).await;
    assert_eq!(functions["weight"], 6);
    let first = &functions["rows"][0];
    assert_eq!(
        functions["keys"]
            [usize::try_from(first["key"].as_u64().expect("a key")).expect("an index")]["name"],
        "leaf"
    );
    assert_eq!(
        (first["own"].clone(), first["total"].clone()),
        (json!(6), json!(6))
    );

    // Each sample falls in one slice of one thread's lane.
    let timeline = tab
        .ok("profileTimeline", json!({"profile": id, "buckets": 10}))
        .await;
    let lanes = timeline["lanes"].as_array().expect("lanes");
    assert_eq!(lanes.len(), 2);
    let weights: u64 = lanes
        .iter()
        .flat_map(|lane| lane["weights"].as_array().expect("weights").iter())
        .map(|weight| weight.as_u64().expect("a weight"))
        .sum();
    assert_eq!(weights, 6);

    // A transform names something the profile has, or fails.
    let (kind, message) = tab
        .with_bytes(
            "profileTree",
            json!({"profile": id, "transforms": [{"op": "merge", "name": "nothing"}]}),
        )
        .await
        .expect_err("an unknown name");
    assert_eq!(kind, "invalid");
    assert!(message.contains("nothing"), "{message}");
    tab.save_traffic("profile");
}

/// The recursion fixture, a sample at its calls of work, collapsed to the
/// first call of each function on every path. The kernel throttles some
/// of the calls, so each branch's weight is checked against the plain tree.
#[tokio::test]
async fn collapsing_cycles_cuts_each_path_back_to_its_first_call() {
    let scratch = ScratchDir::new("web-cycles");
    let mut command = Command::new(Scenario::fixture("profile-recursion"));
    command.arg("20");
    let options = RecordOptions {
        source: SampleSource::Function {
            symbol: "work".into(),
        },
        function_period: 1,
        ..RecordOptions::default()
    };
    let recording = record(command, &options).expect("record");
    let path = scratch.path().join("recursion.usprof");
    recording.profile.write(&path).expect("write the profile");

    let web = Web::start("profile", &[path.to_str().expect("a path")]);
    let mut tab = web.control("tab").await;
    let listed = tab
        .expect("the profiles", |message| message["type"] == "profiles")
        .await;
    let id = listed["profiles"][0]["id"]
        .as_str()
        .expect("an id")
        .to_owned();

    // Below main, each path as `a/b/c` with its own and total weight.
    let below_main = |nodes: &[(Vec<String>, u64, u64)]| {
        nodes
            .iter()
            .filter_map(|(path, own, total)| {
                let main = path.iter().position(|name| name == "main")?;
                (path.len() > main + 1).then(|| (path[main + 1..].join("/"), *own, *total))
            })
            .collect::<std::collections::BTreeSet<_>>()
    };
    let total_of = |nodes: &[(Vec<String>, u64, u64)], name: &str| {
        below_main(nodes)
            .iter()
            .find(|(path, _, _)| path == name)
            .map_or(0, |(_, _, total)| *total)
    };

    let plain = tree(&mut tab, json!({"profile": id})).await;
    let (selfs, visits) = (total_of(&plain, "self"), total_of(&plain, "visit"));
    assert!(selfs > 0 && visits > 0, "{plain:?}");
    assert!(
        plain.iter().any(|(path, _, _)| path.len() > 20),
        "{plain:?}"
    );

    let collapsed = tree(
        &mut tab,
        json!({"profile": id, "transforms": [{"op": "collapseCycles"}]}),
    )
    .await;
    assert_eq!(collapsed[0].2, plain[0].2);
    assert_eq!(
        below_main(&collapsed),
        [
            ("self".to_owned(), 0, selfs),
            ("self/work".to_owned(), selfs, selfs),
            ("visit".to_owned(), 0, visits),
            ("visit/children".to_owned(), 0, visits),
            ("visit/children/work".to_owned(), visits, visits),
        ]
        .into_iter()
        .collect()
    );
}

/// A profile of the waits fixture with task snapshots, once two of them
/// have found its three receivers waiting, one after a snapshot found the
/// spinner with its labels.
fn record_waits(scratch: &ScratchDir) -> std::path::PathBuf {
    let (reader, mut writer) = std::io::pipe().expect("pipe");
    let mut command = Command::new(Scenario::fixture("profile-go-waits"));
    command.stdin(reader).stdout(Stdio::null());
    let options = RecordOptions {
        task_snapshots: Some(Duration::from_millis(50)),
        ..RecordOptions::default()
    };

    let (stop, mut released, mut labeled) = (AtomicBool::new(false), false, None);

    let recording = record_watched(
        command,
        &options,
        &mut Watch {
            every: Duration::from_millis(20),
            publish: &mut |profile| {
                let found = profile
                    .task_waits()
                    .iter()
                    .filter(|wait| wait.state == "chan receive")
                    .count();
                let spins = || {
                    profile
                        .tasks()
                        .iter()
                        .any(|task| task.labels == [("phase".to_owned(), "spin".to_owned())])
                };

                if labeled.is_none() && spins() {
                    labeled = Some(found);
                }

                if !released && labeled.is_some_and(|seen| found >= 6.max(seen + 3)) {
                    writer.write_all(b"x").expect("release the program");
                    released = true;
                }
            },
            stop: &stop,
            save: None,
        },
    )
    .expect("record");
    assert!(recording.status.success());
    let path = scratch.path().join("waits.usprof");
    recording.profile.write(&path).expect("write the profile");
    path
}

#[tokio::test]
async fn a_profile_shows_where_tasks_waited_and_what_each_ran() {
    let scratch = ScratchDir::new("web-waits");
    let path = record_waits(&scratch);
    let web = Web::start("profile", &[path.to_str().expect("a path")]);
    let mut tab = web.control("tab").await;
    let listed = tab
        .expect("the profiles", |message| message["type"] == "profiles")
        .await;
    let id = listed["profiles"][0]["id"]
        .as_str()
        .expect("an id")
        .to_owned();

    let summary = tab.ok("profile", json!({"profile": id})).await;
    assert!(summary["snapshots"].as_u64() >= Some(2), "{summary}");
    let tasks = summary["tasks"].as_array().expect("tasks");
    let spinner = tasks
        .iter()
        .find(|task| {
            task["name"]
                .as_str()
                .is_some_and(|name| name.ends_with("{phase=spin}"))
        })
        .unwrap_or_else(|| panic!("the spinner: {tasks:?}"));
    assert!(spinner["weight"].as_u64() > Some(0), "{spinner}");
    assert!(summary["taskSamples"].as_u64() > Some(0));

    // The waits weigh what every task waited, and each receiver's root
    // is its own task by task.
    let waits = tree(&mut tab, json!({"profile": id, "source": "waits"})).await;
    let roots: u64 = waits
        .iter()
        .filter(|(path, _, _)| path.len() == 1)
        .map(|(_, _, total)| total)
        .sum();
    assert_eq!(Some(roots), summary["waited"].as_u64());
    assert!(
        waits
            .iter()
            .any(|(path, _, _)| path.iter().any(|name| name == "main.receive"))
    );
    let by_task = tree(
        &mut tab,
        json!({"profile": id, "source": "waits", "byTask": true}),
    )
    .await;
    let receivers = by_task
        .iter()
        .filter(|(path, _, _)| path.len() >= 2 && path.iter().any(|name| name == "main.receive"))
        .map(|(path, _, _)| path[0].clone())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(receivers.len(), 3, "{receivers:?}");
    assert!(receivers.iter().all(|name| name.starts_with("goroutine ")));

    // The spinner's samples alone, in a lane of their own.
    let row = spinner["row"].clone();
    let spun = tree(&mut tab, json!({"profile": id, "tasks": [row]})).await;
    assert!(!spun.is_empty());
    assert!(spun.iter().all(|(path, _, _)| {
        path.iter().any(|name| name == "main.spin")
            || path
                .iter()
                .all(|name| name == "runtime.goexit" || name == "main.main.gowrap2")
    }));
    let timeline = tab
        .ok(
            "profileTimeline",
            json!({"profile": id, "buckets": 10, "taskLanes": true}),
        )
        .await;
    let lanes = timeline["lanes"].as_array().expect("lanes");
    assert!(lanes.iter().any(|lane| lane["task"] == row), "{timeline}");
    assert!(lanes.iter().all(|lane| lane["thread"].is_null()));
}

/// Waits for the profiles list to show `id` as `matches` accepts.
pub async fn listed(
    tab: &mut Client,
    id: &str,
    what: &str,
    matches: impl Fn(&Value) -> bool,
) -> Value {
    loop {
        let message = tab
            .expect(what, |message| message["type"] == "profiles")
            .await;
        if let Some(info) = message["profiles"]
            .as_array()
            .and_then(|profiles| profiles.iter().find(|info| info["id"] == id))
            && matches(info)
        {
            return info.clone();
        }
    }
}

#[tokio::test]
async fn a_recording_grows_live_stops_when_asked_and_downloads_whole() {
    let web = Web::start("record", &[]);
    let mut tab = web.control("tab").await;
    let started = tab
        .ok(
            "recordProfile",
            json!({"program": Scenario::fixture("spin"), "preset": "development"}),
        )
        .await;
    let id = started["profile"].as_str().expect("an id").to_owned();

    // The profile is published as it grows, each time a new revision.
    let mut samples = 0;
    let mut revision = 0;
    while samples == 0 {
        let info = listed(&mut tab, &id, "a newer revision", |info| {
            info["revision"].as_u64() > Some(revision)
        })
        .await;
        assert_eq!(info["recording"], true);
        revision = info["revision"].as_u64().expect("a revision");
        let summary = tab
            .ok("profile", json!({"profile": id, "revision": revision}))
            .await;
        samples = summary["samples"].as_u64().expect("samples");
    }

    let summary = tab.ok("profile", json!({"profile": id})).await;
    let pid: u64 = summary["settings"]
        .as_array()
        .and_then(|settings| settings.iter().find(|setting| setting["name"] == "pid"))
        .and_then(|setting| setting["value"].as_str()?.parse().ok())
        .expect("the program's pid");

    // A viewer may look but not stop it.
    let link = tab.ok("share", json!({"role": "view", "to": "/"})).await;
    let mut viewer = web
        .joining("viewer", link["url"].as_str().expect("a link"))
        .await;
    let (kind, _) = viewer
        .request("stopRecording", json!({"profile": id}))
        .await
        .expect_err("a viewer's stop");
    assert_eq!(kind, "forbidden");

    tab.ok("stopRecording", json!({"profile": id})).await;
    let ended = listed(&mut tab, &id, "the recording's end", |info| {
        info["recording"] == false
    })
    .await;
    assert!(
        ended["ended"]
            .as_str()
            .is_some_and(|ended| ended.contains("SIGKILL")),
        "{ended}"
    );
    crate::web::wait_gone(pid);

    // The file the page saves is the whole profile.
    let summary = tab.ok("profile", json!({"profile": id})).await;
    let cookie = web.cookie(&web.control_token.clone());
    let (head, body) = web.http_bytes(&format!(
        "POST /api/profile/{id} HTTP/1.1\r\nHost: {}\r\nOrigin: {}\r\nCookie: {cookie}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        web.host(),
        web.origin()
    ));
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(head.contains("filename=\"spin.usprof\""), "{head}");
    let saved = microscope::profiler::Profile::from_bytes(&body).expect("a profile");
    assert_eq!(
        Some(saved.profile.samples() as u64),
        summary["samples"].as_u64()
    );
}

#[tokio::test]
async fn a_server_that_ends_ends_its_recordings_programs() {
    let mut web = Web::start("record-shutdown", &[]);
    let mut tab = web.control("tab").await;
    let started = tab
        .ok(
            "recordProfile",
            json!({"program": Scenario::fixture("spin")}),
        )
        .await;
    let id = started["profile"].as_str().expect("an id");
    let summary = tab.ok("profile", json!({"profile": id})).await;
    let pid: u64 = summary["settings"]
        .as_array()
        .and_then(|settings| settings.iter().find(|setting| setting["name"] == "pid"))
        .and_then(|setting| setting["value"].as_str()?.parse().ok())
        .expect("the program's pid");
    assert!(crate::web::exists(pid));
    let status = web.interrupt();
    assert!(status.success(), "{status}");
    crate::web::wait_gone(pid);
}

#[tokio::test]
async fn a_recording_window_must_be_a_number_of_seconds() {
    let web = Web::start("record-window", &[]);
    let mut tab = web.control("tab").await;
    let (kind, message) = tab
        .request(
            "recordProfile",
            json!({"program": Scenario::fixture("spin"), "window": -1.0}),
        )
        .await
        .expect_err("a negative window");
    assert_eq!(kind, "invalid");
    assert!(message.contains("a number of seconds"), "{message}");
}

#[tokio::test]
async fn a_program_that_cannot_start_fails_its_recording() {
    let web = Web::start("record-missing", &[]);
    let mut tab = web.control("tab").await;
    let (kind, message) = tab
        .request("recordProfile", json!({"program": "/nonexistent/program"}))
        .await
        .expect_err("a missing program");
    assert_eq!(kind, "failed");
    assert!(message.contains("starting the program failed"), "{message}");
}

/// A profile of the probes fixture's nested zones and labels.
fn record_zones(scratch: &ScratchDir) -> std::path::PathBuf {
    let mut command = Command::new(Scenario::fixture("probes"));
    command.arg("zones").stdout(Stdio::null());
    let recording = record(command, &RecordOptions::default()).expect("record");
    assert!(recording.status.success());
    let path = scratch.path().join("zones.usprof");
    recording.profile.write(&path).expect("write the profile");
    path
}

/// A zones answer's blocks: thread, depth, name, and zone count.
fn blocks(answer: &Value, bytes: &[u8]) -> Vec<(u64, u32, String, u32)> {
    let count = usize::try_from(answer["blocks"].as_u64().expect("a count")).expect("small");
    assert_eq!(bytes.len(), count * 36);
    let word = |column: usize, at: usize| {
        let offset = count * 16 + (column * count + at) * 4;
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("four bytes"))
    };

    let names = answer["names"].as_array().expect("names");
    (0..count)
        .map(|at| {
            let name = names[word(2, at) as usize].as_str().expect("a name");
            (
                u64::from(word(0, at)),
                word(1, at),
                name.to_owned(),
                word(3, at),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_profile_serves_its_zones_folded_to_a_width_and_measured() {
    let scratch = ScratchDir::new("web-zones");
    let path = record_zones(&scratch);
    let web = Web::start("profile", &[path.to_str().expect("a path")]);
    let mut tab = web.control("tab").await;
    let listed = tab
        .expect("the profiles", |message| message["type"] == "profiles")
        .await;
    let id = listed["profiles"][0]["id"]
        .as_str()
        .expect("an id")
        .to_owned();

    let summary = tab.ok("profile", json!({"profile": id})).await;
    assert_eq!(summary["probes"]["overwritten"], 0, "{summary}");
    assert_eq!(summary["labelKeys"], json!(["round"]));
    let sites: Vec<(&str, bool, u64)> = summary["zoneSites"]
        .as_array()
        .expect("sites")
        .iter()
        .map(|site| {
            (
                site["name"].as_str().expect("a name"),
                site["label"].as_bool().expect("a kind"),
                site["count"].as_u64().expect("a count"),
            )
        })
        .collect();
    for expected in [
        ("round", true, 3),
        ("outer", false, 3),
        ("leaf", false, 3),
        ("step", false, 2),
    ] {
        assert!(sites.contains(&expected), "{expected:?} in {sites:?}");
    }

    // Wide enough, every zone is a block of its own, at its depth.
    let (answer, bytes) = tab
        .with_bytes(
            "profileZones",
            json!({"profile": id, "width": 1_000_000_000}),
        )
        .await
        .expect("zones");
    let drawn = blocks(&answer, &bytes);
    assert_eq!(drawn.len(), 18, "{drawn:?}");
    assert!(drawn.iter().all(|(_, _, _, count)| *count == 1));
    let depth_of = |name: &str| {
        drawn
            .iter()
            .filter(|(_, _, drawn, _)| drawn == name)
            .map(|(_, depth, _, _)| *depth)
            .collect::<Vec<_>>()
    };

    assert_eq!(depth_of("round=1"), [0]);
    assert_eq!(depth_of("leaf"), [3, 3, 3], "{drawn:?}");
    assert_eq!(answer["marks"].as_array().map(Vec::len), Some(7));

    // Each round's plotted value, in a bin of its own.
    let plots = answer["plots"].as_array().expect("plots");
    assert_eq!(plots.len(), 1, "{plots:?}");
    assert_eq!(
        (&plots[0]["name"], &plots[0]["values"]),
        (&json!("round"), &json!(3))
    );
    let lows: Vec<f64> = plots[0]["bins"]
        .as_array()
        .expect("bins")
        .iter()
        .map(|bin| bin[3].as_f64().expect("a value"))
        .collect();
    assert_eq!(lows, [0.0, 1.0, 2.0]);

    // Ten pixels wide, each thread's track folds into a block per depth.
    let (answer, bytes) = tab
        .with_bytes("profileZones", json!({"profile": id, "width": 10}))
        .await
        .expect("zones");
    let folded = blocks(&answer, &bytes);
    assert_eq!(
        folded.iter().map(|block| block.3).sum::<u32>(),
        18,
        "{folded:?}"
    );
    assert!(folded.len() < drawn.len());

    let stats = tab
        .ok("profileZoneStats", json!({"profile": id, "name": "round"}))
        .await;
    assert_eq!(stats["count"], 3, "{stats}");
    assert!(stats["median"].as_u64() <= stats["max"].as_u64());

    let values: Vec<&str> = stats["values"]
        .as_array()
        .expect("values")
        .iter()
        .map(|value| value["value"].as_str().expect("a value"))
        .collect();
    assert_eq!(values.len(), 3);

    for value in ["0", "1", "2"] {
        assert!(values.contains(&value), "{values:?}");
    }

    // A label is a query's filter too: one no sample carries keeps none.
    let filtered = tree(&mut tab, json!({"profile": id, "labels": ["round=9"]})).await;
    assert!(filtered.is_empty());
    let failed = tab
        .with_bytes("profileTree", json!({"profile": id, "labels": ["round"]}))
        .await;
    assert!(failed.is_err());
}

/// A function's samples are served on its source lines, with the text
/// around them, and on its instructions.
#[tokio::test]
async fn a_function_is_served_on_its_lines_and_its_instructions() {
    let scratch = ScratchDir::new("web-annotate");
    let path = record_calls(&scratch);
    let web = Web::start("profile", &[path.to_str().expect("a path")]);
    let mut tab = web.control("tab").await;
    let listed = tab
        .expect("the profiles", |message| message["type"] == "profiles")
        .await;
    let id = listed["profiles"][0]["id"]
        .as_str()
        .expect("an id")
        .to_owned();

    let lines = tab
        .ok("profileLines", json!({"profile": id, "function": "leaf"}))
        .await;
    assert_eq!(lines["weight"], 6);
    let file = &lines["files"][0];
    let sampled = file["lines"].as_array().expect("lines");
    assert_eq!(sampled.len(), 1, "{lines}");
    assert_eq!(sampled[0][1], 6);
    let first = file["first"].as_u64().expect("a first line");
    let at = usize::try_from(sampled[0][0].as_u64().expect("a line") - first).expect("a line");
    let text = file["text"].as_array().expect("the file's text");
    assert!(
        text[at]
            .as_str()
            .is_some_and(|line| !line.trim().is_empty()),
        "{lines}"
    );

    let code = tab
        .ok("profileCode", json!({"profile": id, "function": "leaf"}))
        .await;
    assert!(code["unavailable"].is_null(), "{code}");
    assert_eq!(code["blocks"][0]["symbol"], "leaf");
    assert_eq!(code["blocks"][0]["instructions"][0]["own"], 6);
    let unknown = tab
        .ok("profileCode", json!({"profile": id, "function": "nothing"}))
        .await;
    assert!(
        unknown["unavailable"]
            .as_str()
            .is_some_and(|why| why.contains("no selected sample"))
    );
}
