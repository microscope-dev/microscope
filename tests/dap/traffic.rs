use std::collections::HashMap;

use serde_json::Value;

use crate::dap::{Dap, Sent, fixture};
use crate::support::ExternalProcess;

const ID_FIELDS: [&str; 3] = ["threadId", "frameId", "variablesReference"];

const AWAITED_EVENTS: [&str; 4] = ["initialized", "stopped", "exited", "terminated"];

#[derive(Default)]
struct Ids(HashMap<i64, i64>);

impl Ids {
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

struct Target {
    process: ExternalProcess,
    released: bool,
}

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
        assert_eq!(
            shown(&live["body"]),
            shown(&recorded["body"]),
            "{} showed something else than when recorded:\nrecorded {recorded}\nlive {live}",
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
        assert_eq!(
            shown(&live),
            shown(&recorded["body"]),
            "{event} showed something else than when recorded:\nrecorded {recorded}\nlive {live}"
        );
        ids.learn(&recorded["body"], &live, "", "");
    }
}

fn shown(body: &Value) -> Vec<String> {
    let mut shown = Vec::new();
    for key in ["reason", "result"] {
        if let Some(text) = body[key].as_str() {
            shown.push(format!("{key} {text}"));
        }
    }

    for frame in body["stackFrames"].as_array().into_iter().flatten() {
        shown.push(format!("frame {} {}", frame["name"], frame["line"]));
    }

    for variable in body["variables"].as_array().into_iter().flatten() {
        shown.push(format!(
            "variable {} {}",
            variable["name"], variable["value"]
        ));
    }

    shown
}

fn replay(name: &str) {
    replay_with(name, None);
}

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

const CHILD_LABEL: &str = "[member session 1] ";

type Messages = Vec<(bool, Value)>;

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

struct Replay {
    messages: Messages,
    ids: Ids,
    sent: HashMap<u64, Sent>,
    seen: usize,
    next: usize,

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

    fn run(&mut self, dap: &mut Dap, target: &mut Option<Target>, before: Option<(&str, usize)>) {
        let awaited = |message: &Value| {
            message["type"] == "response"
                || AWAITED_EVENTS.contains(&message["event"].as_str().unwrap_or_default())
        };

        while self.next < self.messages.len() {
            let index = self.next;
            let (from_client, message) = &self.messages[index];
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

    dap.close_stdin();
    dap.finish();

    if let Some(target) = target {
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
