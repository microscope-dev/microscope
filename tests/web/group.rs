//! A program's processes, debugged together from the page: the members a
//! state lists, the one it describes, and selecting and detaching them.

use serde_json::{Value, json};

use crate::support::Scenario;
use crate::web::{Client, Web};

fn tree() -> String {
    Scenario::fixture("process-tree").display().to_string()
}

/// The member numbered `number` in a state's group.
fn member(state: &Value, number: u64) -> Value {
    state["group"]["members"]
        .as_array()
        .and_then(|members| members.iter().find(|member| member["number"] == number))
        .cloned()
        .unwrap_or_else(|| panic!("no member [{number}] in {state}"))
}

/// Waits until the group stops in member `number`.
async fn stopped_in(tab: &mut Client, number: u64) -> Value {
    tab.state(&format!("the group stopped in [{number}]"), |state| {
        state["inferior"]["state"] == "stopped"
            && state["group"]["stopped"] == number
            && state["group"]["selected"] == number
    })
    .await
}

#[tokio::test]
async fn fork_children_join_the_page_and_each_stops_where_it_hits() {
    let web = Web::start("group", &[&tree(), "--", "2", "direct", "serial"]);
    let mut tab = web.control("tab").await;
    let loaded = tab
        .state("the program loaded", |state| state["session"].is_string())
        .await;

    // One process before the program runs.
    let members = loaded["group"]["members"].as_array().expect("members");
    assert_eq!(members.len(), 1, "{loaded}");
    assert_eq!(members[0]["joined"], "launched");
    assert_eq!(members[0]["state"], "notStarted");
    assert_eq!(loaded["group"]["selected"], 1);

    let id = tab
        .ok("addBreakpoint", json!({"location": "child_work"}))
        .await["id"]
        .clone();
    tab.ok("continue", json!({})).await;

    // The first child stops at the group's breakpoint, under its id.
    let first = stopped_in(&mut tab, 2).await;
    let child = member(&first, 2);
    assert_eq!(child["joined"], "forked", "{first}");
    assert_eq!(child["parent"], 1);
    assert_eq!(child["state"], "stopped");
    assert!(
        child["command"]
            .as_str()
            .is_some_and(|command| command.contains("process-tree 2 direct serial")),
        "{child}"
    );
    assert_eq!(member(&first, 1)["state"], "stopped", "{first}");
    assert_eq!(first["inferior"]["pid"], child["pid"]);
    assert_eq!(first["inferior"]["reason"]["kind"], "breakpoint");
    assert_eq!(first["inferior"]["place"]["function"], "child_work");
    assert_eq!(first["breakpoints"][0]["id"], id, "{first}");
    let log = tab.output_until("log", "stopped the group").await;
    assert!(log.contains("[2] process "), "{log}");
    assert!(log.contains("joined, started by [1]"), "{log}");

    // The stack is the child's.
    let (stop, thread) = (
        first["inferior"]["stop"].clone(),
        first["inferior"]["thread"].clone(),
    );
    let trace = tab
        .ok("backtrace", json!({"stop": stop, "thread": thread}))
        .await;
    assert_eq!(trace["frames"][0]["name"], "child_work", "{trace}");

    // Selecting the program's own process shows it, waiting for its child.
    tab.ok("selectMember", json!({"member": 1})).await;
    let root = tab
        .state("the root selected", |state| {
            state["group"]["selected"] == 1 && state["inferior"]["state"] == "stopped"
        })
        .await;
    assert_eq!(root["inferior"]["pid"], member(&root, 1)["pid"]);
    assert_eq!(root["group"]["stopped"], 2, "{root}");
    let (kind, _) = tab
        .request("selectMember", json!({"member": 9}))
        .await
        .expect_err("no member 9");
    assert_eq!(kind, "invalid");

    // Continuing runs the whole group, until the second child stops.
    let stop = root["inferior"]["stop"].clone();
    tab.ok("continue", json!({"stop": stop})).await;
    let second = stopped_in(&mut tab, 3).await;
    assert_eq!(member(&second, 2)["state"], "exited", "{second}");
    assert_eq!(second["inferior"]["place"]["function"], "child_work");

    let stop = second["inferior"]["stop"].clone();
    tab.ok("continue", json!({"stop": stop})).await;
    let exited = tab
        .state("the program exited", |state| {
            state["inferior"]["state"] == "exited" && state["group"]["selected"] == 1
        })
        .await;
    assert_eq!(member(&exited, 3)["state"], "exited", "{exited}");
    tab.save_traffic("group");
}

#[tokio::test]
async fn a_detached_member_runs_on_alone() {
    let web = Web::start("group-detach", &[&tree(), "--", "1", "direct"]);
    let mut tab = web.control("tab").await;
    tab.state("the program loaded", |state| state["session"].is_string())
        .await;
    tab.ok("addBreakpoint", json!({"location": "child_work"}))
        .await;
    tab.ok("continue", json!({})).await;
    stopped_in(&mut tab, 2).await;

    let (kind, _) = tab
        .request("detachMember", json!({"member": 1}))
        .await
        .expect_err("the root stays");
    assert_eq!(kind, "invalid");

    tab.ok("detachMember", json!({"member": 2})).await;
    let detached = tab
        .state("[2] detached", |state| {
            state["group"]["selected"] == 1 && member(state, 2)["state"] == "detached"
        })
        .await;
    tab.expect("the notice", |message| {
        message["type"] == "notice"
            && message["text"]
                .as_str()
                .is_some_and(|text| text.starts_with("detached [2]"))
    })
    .await;

    // The child exits 0 on its own, so the program does too.
    let stop = detached["inferior"]["stop"].clone();
    tab.ok("continue", json!({"stop": stop})).await;
    let exited = tab.inferior("exited").await;
    assert!(
        exited["inferior"]["description"]
            .as_str()
            .is_some_and(|text| text.contains("status 0")),
        "{exited}"
    );
}

/// The members `group-spin` makes: its own process and the four it forks.
const SPINNING: usize = 5;

/// A group of spinning members, stopped by a pause, with member `selected`
/// shown, and a second tab on it.
async fn spinning(name: &str, selected: impl Fn(&Value) -> u64) -> (Web, Client, Client, u64) {
    let web = Web::start(
        name,
        &[&Scenario::fixture("group-spin").display().to_string()],
    );
    let mut a = web.control("a").await;
    let b = web.control("b").await;
    a.state("the program loaded", |state| state["session"].is_string())
        .await;
    a.ok("continue", json!({})).await;
    let all = a
        .state("every member running", |state| {
            state["group"]["members"].as_array().is_some_and(|members| {
                members.len() == SPINNING
                    && members.iter().all(|member| member["state"] == "running")
            })
        })
        .await;
    let number = selected(&all);
    a.ok("pause", json!(null)).await;
    a.inferior("stopped").await;
    a.ok("selectMember", json!({"member": number})).await;
    (web, a, b, number)
}

/// The answer to request `id`, which `tab` sent without waiting for it.
async fn answer(tab: &mut Client, id: u64) -> Result<Value, String> {
    let answer = tab
        .expect("an answer", |message| {
            message["id"] == id && matches!(message["type"].as_str(), Some("result" | "error"))
        })
        .await;
    if answer["type"] == "result" {
        Ok(answer["result"].clone())
    } else {
        Err(answer["error"]["kind"]
            .as_str()
            .unwrap_or_default()
            .to_owned())
    }
}

/// Two tabs act on the stop both show, one continuing the group and one
/// stepping the member shown. Only the first to reach that member may
/// resume it: a continue that lost must not resume the step's stop, nor
/// the rest of the group without it.
async fn continue_races_a_step(name: &str, selected: impl Fn(&Value) -> u64) {
    let (_web, mut a, mut b, number) = spinning(name, selected).await;

    let mut previous = Value::Null;
    for round in 0..4 {
        let stopped = a
            .state("a stop of the member shown", |state| {
                state["inferior"]["state"] == "stopped"
                    && state["group"]["selected"] == number
                    && state["inferior"]["stop"] != previous
            })
            .await;
        let (stop, thread) = (
            stopped["inferior"]["stop"].clone(),
            stopped["inferior"]["thread"].clone(),
        );

        let continue_request = a.send("continue", json!({"stop": stop})).await;
        let step_request = b
            .send(
                "step",
                json!({"stop": stop, "thread": thread, "kind": "instruction"}),
            )
            .await;
        let step_answer = answer(&mut b, step_request).await;
        let continue_answer = answer(&mut a, continue_request).await;
        assert!(
            step_answer.is_ok() != continue_answer.is_ok(),
            "round {round}: from stop {stop}, the step answered {step_answer:?} and the continue {continue_answer:?}"
        );

        if step_answer.is_ok() {
            // The group stays stopped at the step's stop, which is still
            // there to continue from.
            let landed = a
                .state("the step's stop", |state| {
                    state["inferior"]["state"] == "stopped" && state["inferior"]["stop"] != stop
                })
                .await;
            assert_eq!(landed["inferior"]["reason"]["kind"], "step", "{landed}");
            previous = landed["inferior"]["stop"].clone();
            a.request("continue", json!({"stop": previous}))
                .await
                .unwrap_or_else(|error| {
                    panic!("round {round}: the step's stop {previous} was resumed: {error:?}")
                });
        } else {
            previous = stop;
        }

        a.inferior("running").await;
        a.ok("pause", json!(null)).await;
    }
}

#[tokio::test]
async fn a_continue_that_loses_to_a_step_resumes_no_member_resumed_last() {
    continue_races_a_step("group-race-last", |state| {
        state["group"]["members"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|member| member["number"].as_u64())
            .max()
            .unwrap_or(1)
    })
    .await;
}

#[tokio::test]
async fn a_continue_that_loses_to_a_step_resumes_no_member_resumed_first() {
    continue_races_a_step("group-race-first", |_| 1).await;
}
