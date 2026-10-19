//! Replays the traffic of real clients against the adapter.
//!
//! `just uat-vscode` and `just uat-nvim` record what VS Code and nvim-dap
//! exchange with the adapter while driving it as users do. Replaying a
//! recording sends each client message in its recorded order, once the
//! responses and stops the client had seen by then have arrived, with the
//! live adapter's thread, frame, and variable ids in place of the recorded
//! ones. The harness validates every live message as in every scenario, and
//! each request must succeed or fail as it did when recorded.

use std::collections::HashMap;

use serde_json::Value;

use crate::dap::{Configuration, Dap, Sent, fixture};
use crate::support::ExternalProcess;

/// Fields whose values the adapter assigns and the client echoes.
const ID_FIELDS: [&str; 3] = ["threadId", "frameId", "variablesReference"];

/// Events a client waits for before its next requests.
const AWAITED_EVENTS: [&str; 4] = ["initialized", "stopped", "exited", "terminated"];

/// Recorded ids and the live ids that stand for them.
#[derive(Default)]
struct Ids(HashMap<i64, i64>);

impl Ids {
    /// Learns the live ids in a live message shaped like a recorded one.
    fn learn(&mut self, recorded: &Value, live: &Value, field: &str, in_list: &str) {
        match (recorded, live) {
            (Value::Object(recorded), Value::Object(live)) => {
                for (key, value) in recorded {
                    if let Some(other) = live.get(key) {
                        self.learn(value, other, key, in_list);
                    }
                }

                let identifies = matches!(in_list, "stackFrames" | "threads");
                if let (true, Some(recorded), Some(live)) = (
                    identifies,
                    recorded.get("id").and_then(Value::as_i64),
                    live.get("id").and_then(Value::as_i64),
                ) {
                    self.0.insert(recorded, live);
                }
            }
            (Value::Array(recorded), Value::Array(live)) => {
                for (recorded, live) in recorded.iter().zip(live) {
                    self.learn(recorded, live, field, field);
                }
            }
            (Value::Number(recorded), Value::Number(live)) if ID_FIELDS.contains(&field) => {
                if let (Some(recorded), Some(live)) = (recorded.as_i64(), live.as_i64()) {
                    self.0.insert(recorded, live);
                }
            }
            _ => {}
        }
    }

    /// Replaces the recorded ids a client message echoes.
    fn rewrite(&self, message: &mut Value) {
        match message {
            Value::Object(map) => {
                for (key, value) in map.iter_mut() {
                    match value.as_i64() {
                        Some(id) if ID_FIELDS.contains(&key.as_str()) => {
                            *value = self.0.get(&id).copied().unwrap_or(id).into();
                        }
                        _ => self.rewrite(value),
                    }
                }
            }
            Value::Array(values) => values.iter_mut().for_each(|value| self.rewrite(value)),
            _ => {}
        }
    }
}

/// A process a recording attached to, released to reach its breakpoint
/// when the recording stopped there.
struct Target {
    process: ExternalProcess,
    released: bool,
}

/// Waits for the live counterpart of a recorded adapter message and
/// learns its ids.
fn await_live(
    dap: &mut Dap,
    sent: &HashMap<u64, Sent>,
    recorded: &Value,
    ids: &mut Ids,
    target: &mut Option<Target>,
) {
    if recorded["type"] == "response" {
        let request = recorded["request_seq"].as_u64().expect("request_seq");
        let live = dap.response(sent[&request]);
        assert_eq!(
            live["success"], recorded["success"],
            "{} answered differently than when recorded:\nrecorded {recorded}\nlive {live}",
            recorded["command"]
        );
        ids.learn(&recorded["body"], &live["body"], "", "");
    } else {
        let event = recorded["event"].as_str().expect("event");
        if let Some(target) = target
            .as_mut()
            .filter(|target| event == "stopped" && !target.released)
        {
            target.process.release();
            target.released = true;
        }

        let live = dap.event(crate::dap::Mark::START, event, |_| true);
        ids.learn(&recorded["body"], &live, "", "");
    }
}

fn replay(name: &str) {
    replay_with(name, None, None);
}

/// Replays a recording; one that attached is given a live process in place
/// of the recorded one. With `children`, the harness starts each child
/// session the adapter asks for, configured so, and finishes it last.
fn replay_with(name: &str, mut target: Option<Target>, children: Option<Configuration>) {
    let mut text = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("tests/dap/traffic/{name}.log")),
    )
    .expect("recording")
    .replace("${root}", env!("CARGO_MANIFEST_DIR"));
    if let Some(target) = &target {
        let recorded = text
            .split("\"pid\":\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("a recorded pid")
            .to_owned();
        text = text.replace(
            &format!("\"pid\":\"{recorded}\""),
            &format!("\"pid\":\"{}\"", target.process.process_id()),
        );
    }

    let messages = text
        .lines()
        .filter_map(|line| {
            let (direction, json) = line.split_once(' ')?;
            let message = serde_json::from_str::<Value>(json).ok()?;
            Some((direction == "<-", message))
        })
        .collect::<Vec<_>>();

    // The adapter messages a client waits for: responses, and the events
    // that change what it may ask.
    let awaited = |message: &Value| {
        message["type"] == "response"
            || AWAITED_EVENTS.contains(&message["event"].as_str().unwrap_or_default())
    };

    let mut dap = Dap::start(format!("replay {name}"));
    let following = children.is_some();
    if let Some(configuration) = children {
        dap.follow_children(configuration);
    }

    let mut ids = Ids::default();
    let mut sent = HashMap::new();
    let mut seen = 0;

    for (index, (from_client, message)) in messages.iter().enumerate() {
        // The harness answers the adapter's reverse requests itself.
        if !from_client || message["type"] != "request" {
            continue;
        }

        while seen < index {
            let (from_client, earlier) = &messages[seen];
            seen += 1;
            if !from_client && awaited(earlier) {
                await_live(&mut dap, &sent, earlier, &mut ids, &mut target);
            }
        }

        let mut message = message.clone();
        ids.rewrite(&mut message);
        let seq = message["seq"].as_u64().expect("seq");
        let command = message["command"].as_str().expect("command");
        sent.insert(seq, dap.send_raw(seq, command, &message.to_string()));

        // Anything a client sends after it disconnects reaches no adapter.
        if command == "disconnect" {
            seen = index;
            break;
        }
    }

    for (from_client, earlier) in &messages[seen..] {
        if !from_client && awaited(earlier) {
            await_live(&mut dap, &sent, earlier, &mut ids, &mut target);
        }
    }

    // The recording ends with its client's disconnect.
    let child = following.then(|| dap.child().0);
    dap.close_stdin();
    dap.finish();
    if let Some(child) = child {
        child.finish();
    }

    if let Some(target) = target {
        // Detached, the process finishes on its own.
        assert_eq!(target.process.wait().code(), Some(23));
    }
}

#[test]
fn vscode_launching_stepping_disassembling_and_restarting() {
    replay("vscode-launch");
}

#[test]
fn vscode_running_a_program_in_its_terminal() {
    replay("vscode-terminal");
}

#[test]
fn vscode_attaching_to_a_process() {
    let target = Target {
        process: ExternalProcess::spawn(&fixture("attach")),
        released: false,
    };

    replay_with("vscode-attach", Some(target), None);
}

#[test]
fn vscode_following_a_forked_child() {
    // The child's session runs it to its end, which its parent waits for.
    replay_with("vscode-fork", None, Some(Configuration::default()));
}

#[test]
fn vscode_opening_a_core_dump() {
    replay("vscode-core");
}

#[test]
fn vscode_stepping_a_goroutine_into_a_panic() {
    replay("vscode-go");
}

#[test]
fn vscode_showing_a_stack_that_crosses_stacks() {
    replay("vscode-go-stacks");
}

#[test]
fn nvim_dap_launching_stepping_and_evaluating() {
    replay("nvim-launch");
}

#[test]
fn nvim_dap_running_a_program_in_its_terminal() {
    replay("nvim-terminal");
}

#[test]
fn nvim_dap_following_a_forked_child() {
    replay_with("nvim-fork", None, Some(Configuration::default()));
}
