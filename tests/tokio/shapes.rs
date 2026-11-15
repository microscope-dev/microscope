use std::process::Stdio;

use microscope::{BreakpointSpec, CodeRole, LaunchOptions, LineNumber, StepKind, StopReason};

use crate::support::Scenario;

use crate::invariants::checked;
use crate::stops::{line, place};

const BUILDS: [&str; 4] = [
    "tokio-shapes-o0",
    "tokio-shapes-o3",
    "tokio-shapes-1.51-o0",
    "tokio-shapes-1.51-o3",
];
const SOURCE: &str = "shapes/src/main.rs";

async fn stopped_at(scenario: &mut Scenario, markers: &[&str]) -> StopReason {
    for marker in markers {
        scenario
            .add_breakpoint_spec(BreakpointSpec::Source {
                path: SOURCE.into(),
                line: LineNumber::new(line(SOURCE, marker)).expect("one-based"),
            })
            .await;
    }

    scenario
        .run_with_to_stop(LaunchOptions {
            stdout: Some(Stdio::null()),
            ..LaunchOptions::default()
        })
        .await
}

async fn step_in(scenario: &mut Scenario, context: &str) {
    let kind = StepKind::IntoSource;
    assert_eq!(
        scenario.step_to_stop(kind).await,
        StopReason::Step { kind },
        "{context}"
    );
}

#[tokio::test]
async fn step_into_an_await_stops_at_the_futures_first_line() {
    for fixture in BUILDS {
        let mut scenario = checked(fixture);
        let awaits = [
            ("// INTO: plain", "plain", "// STEP: plain"),
            (
                "// INTO: block",
                "shapes::{async block#0}",
                "// STEP: block",
            ),
            (
                "// INTO: boxed",
                "shapes::{async block#1}",
                "// STEP: boxed",
            ),
            ("// INTO: generic", "generic", "// STEP: generic"),
            ("// INTO: method", "method", "// STEP: method"),
            ("// INTO: inherent", "inherent", "// STEP: inherent"),
            ("// INTO: recv", "shapes", "// STEP: recv-after"),
        ];

        let mut reason = stopped_at(&mut scenario, &awaits.map(|(from, _, _)| from)).await;

        for (from, function, to) in awaits {
            let context = format!("{fixture} {from}");
            assert!(
                matches!(reason, StopReason::Breakpoint { .. }),
                "{context}: {reason:?}"
            );
            assert_eq!(
                place(&scenario).await,
                ("shapes".to_owned(), line(SOURCE, from)),
                "{context}"
            );

            step_in(&mut scenario, &context).await;

            assert_eq!(
                place(&scenario).await,
                (function.to_owned(), line(SOURCE, to)),
                "{context}"
            );

            if to != "// STEP: recv-after" {
                reason = scenario.resume_to_stop().await;
            }
        }

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn step_into_the_runtime_is_a_choice() {
    let fixture = "tokio-shapes-o0";
    let mut scenario = checked(fixture);
    let reason = stopped_at(&mut scenario, &["// INTO: recv", "// INTO: recv-again"]).await;
    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{reason:?}"
    );

    let entered = scenario
        .operation(
            "step into runtime",
            scenario.handle().set_step_into_runtime(true),
        )
        .await;

    assert!(!entered, "steps skip runtime code by default");

    step_in(&mut scenario, "on").await;

    let location = scenario
        .operation("location", scenario.handle().current_location())
        .await;
    let function = location.image.function.clone().expect("a function");

    assert_eq!(
        (&*function.name, function.role),
        ("recv", CodeRole::RuntimeInternal),
        "{location:?}"
    );

    let entered = scenario
        .operation(
            "step into runtime",
            scenario.handle().set_step_into_runtime(false),
        )
        .await;

    assert!(entered);

    let reason = scenario.resume_to_stop().await;
    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{reason:?}"
    );

    assert_eq!(
        place(&scenario).await,
        ("shapes".to_owned(), line(SOURCE, "// INTO: recv-again"))
    );

    step_in(&mut scenario, "off").await;

    assert_eq!(
        place(&scenario).await,
        (
            "shapes".to_owned(),
            line(SOURCE, "// STEP: recv-again-after")
        )
    );

    scenario.shutdown().await;
}
