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

use crate::dap::{Dap, Sent, fixture};
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
    replay_with(name, None);
}

/// A recording's messages, each marked whether its client sent it: the
/// session's own, and those of the forked child's session if it followed
/// one. A client's own recording of the child's session is the `.child`
/// recording beside it; the adapter's log labels the child's messages.
fn recording(name: &str, target: Option<&Target>) -> (Messages, Option<Messages>) {
    let read = |name: &str| {
        std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("tests/dap/traffic/{name}.log")),
        )
        .ok()
        .map(|text| text.replace("${root}", env!("CARGO_MANIFEST_DIR")))
    };
    let mut text = read(name).expect("recording");
    if let Some(target) = target {
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

    let messages = |lines: &mut dyn Iterator<Item = &str>| {
        lines
            .filter_map(|line| {
                let (direction, json) = line.split_once(' ')?;
                let message = serde_json::from_str::<Value>(json).ok()?;
                Some((direction == "<-", message))
            })
            .collect::<Messages>()
    };
    let session = messages(&mut text.lines());
    let labelled = messages(
        &mut text
            .lines()
            .filter_map(|line| line.strip_prefix(CHILD_LABEL)),
    );
    let child = read(&format!("{name}.child"))
        .map(|text| messages(&mut text.lines()))
        .or_else(|| (!labelled.is_empty()).then_some(labelled));
    (session, child)
}

/// The label the adapter's log gives the first member session's messages.
const CHILD_LABEL: &str = "[member session 1] ";

type Messages = Vec<(bool, Value)>;

/// The request that removed a session's breakpoints, as the command and
/// how many requests for it the session had sent by then, counting it: the
/// first that empties a list an earlier request for it filled.
fn removal(messages: &Messages) -> Option<(String, usize)> {
    let mut counts = HashMap::<&str, usize>::new();
    let mut filled = std::collections::HashSet::new();
    messages
        .iter()
        .filter(|(from_client, message)| *from_client && message["type"] == "request")
        .find_map(|(_, message)| {
            let command = message["command"].as_str()?;
            let count = counts.entry(command).or_default();
            *count += 1;
            let breakpoints = message["arguments"]["breakpoints"].as_array()?;
            if !breakpoints.is_empty() {
                filled.insert(command);
                return None;
            }
            filled
                .contains(command)
                .then(|| (command.to_owned(), *count))
        })
}

/// A child's recording with the live member's process and socket in place
/// of the recorded ones, which its attach names.
fn as_live_member(mut messages: Messages, live: &Value) -> Messages {
    for (from_client, message) in &mut messages {
        if *from_client && message["command"] == "attach" {
            let arguments = &mut message["arguments"];
            for key in ["pid", "name", "__microscopeMember"] {
                arguments[key] = live[key].clone();
            }
        }
    }
    messages
}

/// One session's recording, replayed from where it was left.
struct Replay {
    messages: Messages,
    ids: Ids,
    sent: HashMap<u64, Sent>,
    /// The adapter messages before this one have been awaited.
    seen: usize,
    /// The client messages before this one have been sent.
    next: usize,

    /// How many requests for each command have been sent.
    counts: HashMap<String, usize>,
}

impl Replay {
    fn new(messages: Messages) -> Self {
        Self {
            messages,
            ids: Ids::default(),
            sent: HashMap::new(),
            seen: 0,
            next: 0,
            counts: HashMap::new(),
        }
    }

    /// Sends the client's requests in order, each once the messages the
    /// client had seen by then have arrived, up to the `before.1`th request
    /// for command `before.0`, which it leaves unsent; without `before`, to
    /// the end.
    fn run(&mut self, dap: &mut Dap, target: &mut Option<Target>, before: Option<(&str, usize)>) {
        // The adapter messages a client waits for: responses, and the
        // events that change what it may ask.
        let awaited = |message: &Value| {
            message["type"] == "response"
                || AWAITED_EVENTS.contains(&message["event"].as_str().unwrap_or_default())
        };

        while self.next < self.messages.len() {
            let index = self.next;
            let (from_client, message) = &self.messages[index];
            // The harness answers the adapter's reverse requests itself.
            if !from_client || message["type"] != "request" {
                self.next += 1;
                continue;
            }

            let command = message["command"].as_str().expect("command");
            let count = self.counts.get(command).copied().unwrap_or_default() + 1;
            if before == Some((command, count)) {
                return;
            }
            self.counts.insert(command.to_owned(), count);

            while self.seen < index {
                let (from_client, earlier) = &self.messages[self.seen];
                self.seen += 1;
                if !from_client && awaited(earlier) {
                    await_live(dap, &self.sent, earlier, &mut self.ids, target);
                }
            }

            let mut message = message.clone();
            self.ids.rewrite(&mut message);
            let seq = message["seq"].as_u64().expect("seq");
            self.sent
                .insert(seq, dap.send_raw(seq, command, &message.to_string()));
            self.next += 1;

            // Anything a client sends after it disconnects reaches no adapter.
            if command == "disconnect" {
                self.seen = index;
                self.next = self.messages.len();
            }
        }

        for (from_client, earlier) in &self.messages[self.seen..] {
            if !from_client && awaited(earlier) {
                await_live(dap, &self.sent, earlier, &mut self.ids, target);
            }
        }
        self.seen = self.messages.len();
    }
}

/// Replays a recording; one that attached is given a live process in place
/// of the recorded one. A recording with a `.child` recording beside it
/// followed a forked child in a session of its own, which the replay
/// connects to the socket the adapter names. The two sessions' requests go
/// in the order their client sent them: each session to its stop, then
/// each one's breakpoint removal, then the child to its end, and then the
/// parent, which waited for it.
fn replay_with(name: &str, mut target: Option<Target>) {
    let (messages, child) = recording(name, target.as_ref());

    let mut dap = Dap::start(format!("replay {name}"));
    let mut parent = Replay::new(messages);
    if let Some(messages) = child {
        let removed = removal(&parent.messages).expect("the parent's breakpoint removal");
        let child_removed = removal(&messages).expect("the child's breakpoint removal");
        dap.route_children();
        parent.run(&mut dap, &mut target, Some((&removed.0, removed.1)));
        let (mut session, live) = dap.routed_child();
        let mut child = Replay::new(as_live_member(messages, &live));
        child.run(
            &mut session,
            &mut None,
            Some((&child_removed.0, child_removed.1)),
        );
        parent.run(&mut dap, &mut target, Some(("continue", 1)));
        child.run(&mut session, &mut None, None);
        session.close_stdin();
        session.finish();
    }
    parent.run(&mut dap, &mut target, None);

    // The recording ends with its client's disconnect.
    dap.close_stdin();
    dap.finish();

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

    replay_with("vscode-attach", Some(target));
}

#[test]
fn vscode_following_a_forked_child() {
    // The child's session runs it to its end, and then the parent's, which
    // waited for it.
    replay("vscode-fork");
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
    replay("nvim-fork");
}
