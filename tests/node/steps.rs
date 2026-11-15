use microscope::{BreakpointOptions, BreakpointSpec, LineNumber, StepSkips};

use crate::oracle::{self, Command, Pause, Planted, Skipping};
use crate::session::NodeSession;
use crate::support::node;

const FIXTURE: &str = "steps.js";

fn line_of(marker: &str) -> u64 {
    let text = std::fs::read_to_string(node::fixture(FIXTURE)).expect("read the fixture");
    let index = text
        .lines()
        .position(|line| line.contains(&format!("// {marker}")))
        .unwrap_or_else(|| panic!("no line marked {marker}"));
    index as u64 + 1
}

struct Case {
    marker: &'static str,
    commands: &'static [Command],
    skipping: Skipping<'static>,
}

const fn case(marker: &'static str, commands: &'static [Command]) -> Case {
    Case {
        marker,
        commands,
        skipping: Skipping {
            directories: &[],
            enter: false,
        },
    }
}

const CASES: [Case; 10] = [
    case(
        "CALLS",
        &[
            Command::Into,
            Command::Over,
            Command::Out,
            Command::Over,
            Command::Over,
        ],
    ),
    case("LEAF", &[Command::Out, Command::Over]),
    case("COUNTDOWN", &[Command::Over, Command::Over, Command::Into]),
    case(
        "EMITS",
        &[Command::Into, Command::Out, Command::Over, Command::Into],
    ),
    case("MAPS", &[Command::Into, Command::Over, Command::Out]),
    case("AWAITS", &[Command::Over, Command::Over]),
    case("LISTENER", &[Command::Over, Command::Over]),
    case("RELAYS", &[Command::Into, Command::Over, Command::Out]),
    Case {
        marker: "RELAYS",
        commands: &[Command::Into, Command::Out, Command::Over],
        skipping: Skipping {
            directories: &["/vendor/"],
            enter: false,
        },
    },
    Case {
        marker: "EMITS",
        commands: &[Command::Into, Command::Over],
        skipping: Skipping {
            directories: &[],
            enter: true,
        },
    },
];

async fn run(entry: &str, case: &Case) -> (Vec<Pause>, Vec<Pause>) {
    let line = line_of(case.marker);
    let mut expected = oracle::pauses(
        entry,
        FIXTURE,
        &[Planted {
            line,
            condition: None,
        }],
        (case.commands, case.skipping),
        "none",
    );
    expected.truncate(case.commands.len() + 1);

    let spec = BreakpointSpec::Source {
        path: node::fixture(FIXTURE),
        line: LineNumber::new(line).expect("a line"),
    };
    let options = BreakpointOptions {
        pending: true,
        ..BreakpointOptions::default()
    };
    let session = NodeSession::unlaunched(entry, FIXTURE);
    let handle = session.scenario.handle();
    let globs: Vec<_> = case
        .skipping
        .directories
        .iter()
        .map(|directory| format!("**{directory}**"))
        .collect();
    let skips = StepSkips::parse(&globs).expect("skip patterns");
    session
        .scenario
        .operation("skip", handle.set_step_skips(skips))
        .await;
    session
        .scenario
        .operation(
            "enter the runtime",
            handle.set_step_into_runtime(case.skipping.enter),
        )
        .await;
    let (mut session, _, _events) = session.run_to_stop(vec![(spec, options)]).await;
    let mut seen = vec![oracle::selected(&session.scenario).await];
    for command in case.commands {
        session.scenario.step_to_stop(command.kind()).await;
        seen.push(oracle::selected(&session.scenario).await);
    }
    session.shutdown().await;
    (seen, expected)
}

async fn lies(entry: &str, cases: &[Case]) -> Vec<String> {
    let mut lies = Vec::new();
    for case in cases {
        let (seen, expected) = run(entry, case).await;
        let differences = oracle::differences(&seen, &expected);
        if !differences.is_empty() {
            let (marker, commands, skipping) = (case.marker, case.commands, case.skipping);
            lies.push(format!(
                "node {entry} {marker} {commands:?} {skipping:?}:\n  {}",
                differences.join("\n  ")
            ));
        }
    }
    lies
}

async fn steps_end_where_v8_ends_them(entry: &str) {
    let found = lies(entry, &CASES).await;
    assert!(found.is_empty(), "{}", found.join("\n"));
}

#[tokio::test]
async fn javascript_steps_end_where_v8_ends_them_on_26() {
    steps_end_where_v8_ends_them("26").await;
}

#[tokio::test]
async fn javascript_steps_end_where_v8_ends_them_on_24() {
    steps_end_where_v8_ends_them("24").await;
}

#[tokio::test]
async fn javascript_steps_end_where_v8_ends_them_on_22() {
    steps_end_where_v8_ends_them("22").await;
}
