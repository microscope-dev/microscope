//! A client that sends random requests in every state, valid or not.
//!
//! Each run is seeded and deterministic in what it sends; the seed is in
//! the scenario's name, which a failure prints. Whatever is sent, every
//! request must get exactly one well-formed response, the adapter must keep
//! working, and the session must end cleanly. `CHAOS_SEEDS=n` runs `n`
//! other seeds, `CHAOS_SEED=hex` reruns one, and `CHAOS_ROUNDS=n` sends
//! more bursts, for longer runs.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::dap::{Dap, Profile, fixture, source};

/// A small deterministic generator, so no dependency decides the sequence.
struct Random(u64);

impl Random {
    const fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    const fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[usize::try_from(self.below(items.len() as u64)).expect("small index")]
    }

    /// A small number, or now and then an extreme one.
    fn number(&mut self) -> Value {
        match self.below(8) {
            0 => json!(-1),
            1 => json!(i64::from(i32::MAX)),
            2 => json!(0),
            3 => json!("12"),
            _ => json!(self.below(40) + 1),
        }
    }
}

/// Ids the adapter has handed out, which requests use most of the time so
/// that they reach a stopped program's frames and variables.
#[derive(Default)]
struct Known {
    threads: Vec<i64>,
    frames: Vec<i64>,
    references: Vec<i64>,
}

impl Known {
    /// Learns the ids in a successful response.
    fn learn(&mut self, command: &str, body: &Value) {
        let (list, field, ids) = match command {
            "threads" => (&body["threads"], "id", &mut self.threads),
            "stackTrace" => (&body["stackFrames"], "id", &mut self.frames),
            "scopes" => (&body["scopes"], "variablesReference", &mut self.references),
            "variables" => (
                &body["variables"],
                "variablesReference",
                &mut self.references,
            ),
            _ => return,
        };
        let found = list
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| item[field].as_i64())
            .filter(|&id| id > 0)
            .collect::<Vec<_>>();
        if !found.is_empty() {
            *ids = found;
        }
    }
}

impl Random {
    /// One of `known`, three times in four, or else any number.
    fn id(&mut self, known: &[i64]) -> Value {
        if known.is_empty() || self.below(4) == 0 {
            self.number()
        } else {
            json!(*self.pick(known))
        }
    }
}

/// A request that reads a stopped program's stack, frames, or variables.
fn inspection(random: &mut Random, known: &Known) -> (&'static str, Value) {
    match random.below(4) {
        0 => (
            "stackTrace",
            json!({"threadId": random.id(&known.threads), "startFrame": 0, "levels": 20}),
        ),
        1 => ("scopes", json!({"frameId": random.id(&known.frames)})),
        2 => (
            "variables",
            json!({"variablesReference": random.id(&known.references)}),
        ),
        _ => (
            "evaluate",
            json!({"expression": random.pick(&["hot_count", "hot_stop"]), "context": "watch", "frameId": random.id(&known.frames)}),
        ),
    }
}

fn request(random: &mut Random, known: &Known, started: bool) -> (&'static str, Value) {
    let thread = random.id(&known.threads);
    let path = source("c/hot-calls.c");

    // One request in four inspects, as an editor does at each stop.
    if random.below(4) == 0 {
        return inspection(random, known);
    }

    // Before a program starts, a launch or attach would wait for
    // configurationDone like any other.
    match random.below(if started { 35 } else { 32 }) {
        0 => ("threads", Value::Null),
        1 => (
            "stackTrace",
            json!({"threadId": thread, "startFrame": random.number(), "levels": random.number()}),
        ),
        2 => ("scopes", json!({"frameId": random.id(&known.frames)})),
        3 => (
            "variables",
            json!({"variablesReference": random.id(&known.references), "start": random.number(), "count": random.number()}),
        ),
        4 => (
            "evaluate",
            json!({"expression": random.pick(&["hot_count", "hot_stop", "bt", "info signals", "x 0x0", "((", "threads"]), "context": random.pick(&["watch", "repl", "hover"]), "frameId": random.id(&known.frames)}),
        ),
        5 => ("continue", json!({"threadId": thread})),
        6 => (
            "next",
            json!({"threadId": thread, "granularity": random.pick(&["line", "instruction"])}),
        ),
        7 => ("stepIn", json!({"threadId": thread})),
        8 => ("stepOut", json!({"threadId": thread})),
        9 => ("pause", json!({"threadId": thread})),
        10 => (
            "setBreakpoints",
            json!({"source": {"path": path}, "breakpoints": [{"line": random.number()}]}),
        ),
        11 => (
            "setFunctionBreakpoints",
            json!({"breakpoints": [{"name": random.pick(&["hot_function", "caller", "nope", ""])}]}),
        ),
        12 => ("setFunctionBreakpoints", json!({"breakpoints": []})),
        13 => (
            "setExceptionBreakpoints",
            json!({"filters": [random.pick(&["fatal", "other", "routine", "bogus"])]}),
        ),
        14 => (
            "disassemble",
            json!({"memoryReference": random.pick(&["0x0", "0x401000", "junk"]), "instructionOffset": random.number(), "instructionCount": random.below(30)}),
        ),
        15 => (
            "readMemory",
            json!({"memoryReference": random.pick(&["0x0", "0x401000", "junk"]), "count": random.below(100)}),
        ),
        16 => ("modules", json!({})),
        17 => ("loadedSources", Value::Null),
        18 => ("completions", json!({"text": "inf", "column": 4})),
        19 => (
            "dataBreakpointInfo",
            json!({"name": "hot_count", "frameId": random.id(&known.frames)}),
        ),
        20 => (
            "setDataBreakpoints",
            json!({"breakpoints": [{"dataId": random.pick(&["data-1", "bogus"])}]}),
        ),
        21 => ("exceptionInfo", json!({"threadId": thread})),
        22 => ("configurationDone", Value::Null),
        // Ending the program now and then, not so often that it is rarely
        // stopped.
        23 | 24 if random.below(4) != 0 => ("pause", json!({"threadId": thread})),
        23 => ("terminate", Value::Null),
        24 => ("restart", Value::Null),
        25 => (
            "setVariable",
            json!({"variablesReference": random.id(&known.references), "name": random.pick(&["hot_stop", "nope"]), "value": random.pick(&["0", "1", "((", "-1"])}),
        ),
        26 => (
            "setExpression",
            json!({"expression": random.pick(&["hot_count", "hot_stop", "3"]), "value": random.pick(&["0", "7", "x"]), "frameId": random.id(&known.frames)}),
        ),
        27 => (
            "writeMemory",
            json!({"memoryReference": random.pick(&["0x0", "0x401000", "junk"]), "data": random.pick(&["AAAA", "", "!!"])}),
        ),
        28 => ("cancel", json!({"requestId": random.number()})),
        29 => ("locations", json!({"locationReference": random.number()})),
        30 => (
            "microscope/setValueFormat",
            json!({"hex": random.pick(&[json!(true), json!(false), json!("yes")])}),
        ),
        31 => (
            "completions",
            json!({"text": random.pick(&["hot_count.", "$r", "x->", "::h", "info "]), "column": random.number(), "frameId": random.id(&known.frames)}),
        ),
        32 => ("launch", json!({"program": fixture("spin")})),
        33 => ("attach", json!({"pid": random.number()})),
        _ => (
            *random.pick(&["frobnicate", "source", "restartFrame", "goto"]),
            json!({"x": random.number()}),
        ),
    }
}

#[test]
fn random_requests_in_every_state_are_answered_once_and_harm_nothing() {
    let only = std::env::var("CHAOS_SEED")
        .ok()
        .map(|seed| u64::from_str_radix(seed.trim_start_matches("0x"), 16).expect("hex seed"));
    let seeds: Vec<u64> = only.map_or_else(
        || {
            std::env::var("CHAOS_SEEDS").map_or_else(
                |_| vec![0x05ee_d001, 0x05ee_d002, 0x05ee_d003, 0x05ee_d004],
                |count| {
                    (1..=count.parse::<u64>().expect("count"))
                        .map(|seed| seed * 0x9e37_79b9)
                        .collect()
                },
            )
        },
        |seed| vec![seed],
    );

    let mut answered = BTreeMap::new();
    for seed in seeds {
        let mut random = Random(seed);
        let mut dap = Dap::start(format!("chaos seed {seed:#x}"));
        dap.relax_ordering_checks();
        let rounds =
            std::env::var("CHAOS_ROUNDS").map_or(60, |rounds| rounds.parse().expect("rounds"));
        dap.initialize(Profile::VsCode);
        let mut known = Known::default();

        // Before anything is launched.
        bursts(&mut dap, &mut random, 10, false, &mut known, &mut answered);

        // While the launch waits for configuration and starts the program.
        let launch = dap.send("launch", json!({"program": fixture("hot-calls")}));
        let done = dap.send("configurationDone", Value::Null);
        bursts(&mut dap, &mut random, 10, true, &mut known, &mut answered);
        dap.response(launch);
        dap.response(done);

        // While it runs and stops, restarts, or ends.
        bursts(
            &mut dap,
            &mut random,
            rounds,
            true,
            &mut known,
            &mut answered,
        );

        // After it has ended.
        end_program(&mut dap);
        bursts(&mut dap, &mut random, 10, true, &mut known, &mut answered);

        dap.finish();
    }

    // The requests reached a stopped program, read its stack, and resumed
    // it, rather than only being refused while it ran or after it ended.
    for command in ["stackTrace", "continue"] {
        assert!(
            answered.get(command).is_some_and(|&count| count > 0),
            "no {command} succeeded: {answered:?}"
        );
    }
}

/// Sends bursts of random requests without waiting, as editors send them,
/// and checks each is answered once.
fn bursts(
    dap: &mut Dap,
    random: &mut Random,
    rounds: usize,
    started: bool,
    known: &mut Known,
    answered: &mut BTreeMap<&'static str, u32>,
) {
    for _ in 0..rounds {
        let burst = (0..=random.below(3))
            .map(|_| {
                let (command, arguments) = request(random, known, started);
                (command, dap.send(command, arguments))
            })
            .collect::<Vec<_>>();

        for (command, sent) in burst {
            let response = dap.response(sent);
            assert!(response["success"].is_boolean(), "{response}");
            if response["success"] == true {
                known.learn(command, &response["body"]);
                *answered.entry(command).or_default() += 1;
            }
        }
    }
}

/// Whether the latest program has ended.
fn ended(dap: &mut Dap) -> bool {
    let messages = dap.messages_since(crate::dap::Mark::START);
    let started = messages
        .iter()
        .rposition(|message| message["event"] == "process")
        .unwrap_or(0);
    messages[started..]
        .iter()
        .any(|message| message["event"] == "terminated")
}

/// Ends the program unless it has ended. A thread may still stop for a
/// breakpoint or signal before the termination signal arrives; the stop is
/// continued.
fn end_program(dap: &mut Dap) {
    // Events from here on decide; one already on its way is not missed.
    let mark = dap.mark();
    if ended(dap) {
        return;
    }

    // A termination already under way can end the program meanwhile, so
    // any of these may fail; the terminated event decides either way.
    let path = source("c/hot-calls.c");
    for (command, arguments) in [
        (
            "setBreakpoints",
            json!({"source": {"path": path}, "breakpoints": []}),
        ),
        ("setFunctionBreakpoints", json!({"breakpoints": []})),
        ("setDataBreakpoints", json!({"breakpoints": []})),
        ("setExceptionBreakpoints", json!({"filters": []})),
        ("terminate", Value::Null),
    ] {
        let sent = dap.send(command, arguments);
        dap.response(sent);
    }

    loop {
        let (kind, body) = dap.next_event(mark, &["stopped", "terminated"]);
        if kind == "terminated" {
            return;
        }

        // The program may already be gone again.
        let resumed = dap.send("continue", json!({"threadId": body["threadId"]}));
        dap.response(resumed);
    }
}
