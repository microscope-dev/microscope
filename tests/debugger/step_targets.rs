//! Stepping into one chosen call of a line that makes several, as DAP's
//! `stepInTargets` offers.

use microscope::StepTarget;

use super::*;

const SOURCE: &str = "tests/fixtures/c/step-targets.c";

/// The builds a line's calls can differ in.
const BUILDS: [&str; 3] = [
    "step-targets-gcc-o0",
    "step-targets-clang-o0",
    "step-targets-gcc-o2",
];

/// Runs `fixture` to the line `marker` names, and removes the breakpoint
/// that stopped it there, which a recursive call would hit again.
async fn stopped_at(fixture: &str, marker: &str) -> Scenario {
    let mut scenario = Scenario::launch(fixture);
    let line = source_line(SOURCE, marker);
    let breakpoint = scenario.add_source_breakpoint("step-targets.c", line).await;

    assert!(
        matches!(scenario.run_to_stop().await, StopReason::Breakpoint { .. }),
        "{fixture}"
    );

    scenario
        .operation(
            "remove breakpoint",
            scenario.handle().remove_breakpoint(breakpoint.id),
        )
        .await;

    scenario
}

async fn targets(scenario: &Scenario) -> Arc<[StepTarget]> {
    scenario
        .operation("step targets", scenario.handle().step_targets())
        .await
}

fn callees(targets: &[StepTarget]) -> Vec<Option<&str>> {
    targets
        .iter()
        .map(|target| target.callee.as_deref())
        .collect()
}

/// The target calling `callee`.
fn calling<'a>(targets: &'a [StepTarget], callee: &str) -> &'a StepTarget {
    targets
        .iter()
        .find(|target| target.callee.as_deref() == Some(callee))
        .unwrap_or_else(|| panic!("no call to {callee} among {targets:?}"))
}

/// Where the program stopped: its function and line.
async fn place(scenario: &Scenario) -> (String, u64) {
    let location = scenario
        .operation("location", scenario.handle().current_location())
        .await;

    (
        location_function(&location).unwrap_or_default().to_owned(),
        location_line(&location).unwrap_or_default(),
    )
}

async fn signed_variable(scenario: &Scenario, name: &str) -> i128 {
    let variable = scenario
        .operation(name, scenario.handle().variable(name))
        .await;
    match available_value(&variable.state) {
        microscope::VariableValue::Scalar(ScalarValue::Signed(value)) => *value,
        other => panic!("{name} is {other:?}"),
    }
}

/// A line's calls are listed in address order, and stepping into any one
/// of them runs the others it passes to their returns.
#[tokio::test]
async fn a_step_goes_into_the_call_it_names() {
    for fixture in BUILDS {
        let scenario = stopped_at(fixture, "targets: calls").await;
        let listed = targets(&scenario).await;
        let mut names = callees(&listed);
        names.sort_unstable();

        assert_eq!(
            names,
            [Some("add"), Some("inc"), Some("twice")],
            "{fixture}"
        );

        assert!(
            listed.is_sorted_by_key(|target| target.call),
            "{fixture}: {listed:?}"
        );

        scenario.shutdown().await;

        for (callee, parameter, value) in [("add", "a", 2), ("inc", "x", 1), ("twice", "x", 1)] {
            let mut scenario = stopped_at(fixture, "targets: calls").await;
            let call = calling(&targets(&scenario).await, callee).call;

            assert_eq!(
                scenario.step_into_to_stop(call).await,
                StopReason::Step {
                    kind: StepKind::IntoSource
                },
                "{fixture}: {callee}"
            );

            assert_eq!(
                place(&scenario).await,
                (
                    callee.to_owned(),
                    source_line(SOURCE, &format!("targets: {callee}"))
                ),
                "{fixture}"
            );

            // Arguments the line computed by other calls are there.
            assert_eq!(
                signed_variable(&scenario, parameter).await,
                value,
                "{fixture}: {callee}"
            );
            if callee == "add" {
                assert_eq!(signed_variable(&scenario, "b").await, 2, "{fixture}");
            }

            scenario.shutdown().await;
        }
    }
}

/// An indirect call has no callee to name, and stepping into it reaches
/// whatever it calls; a call into code without debug information is
/// stepped through, as a plain step in does, to the next line.
#[tokio::test]
async fn indirect_calls_are_entered_and_undescribed_ones_stepped_through() {
    for fixture in BUILDS {
        let mut scenario = stopped_at(fixture, "targets: indirect").await;
        let listed = targets(&scenario).await;
        let indirect = listed
            .iter()
            .find(|target| target.target.is_none())
            .unwrap_or_else(|| panic!("{fixture}: no indirect call in {listed:?}"));

        assert_eq!(indirect.callee, None, "{fixture}");
        assert!(
            listed
                .iter()
                .any(|target| target.callee.as_deref() == Some("strlen")),
            "{fixture}: {listed:?}"
        );
        scenario.step_into_to_stop(indirect.call).await;

        assert_eq!(
            place(&scenario).await,
            ("inc".to_owned(), source_line(SOURCE, "targets: inc")),
            "{fixture}"
        );

        scenario.shutdown().await;

        // Where a step over the line goes, which optimized code orders.
        let mut scenario = stopped_at(fixture, "targets: indirect").await;
        scenario.step_to_stop(StepKind::OverSource).await;
        let next = place(&scenario).await;
        scenario.shutdown().await;

        let mut scenario = stopped_at(fixture, "targets: indirect").await;
        let strlen = calling(&targets(&scenario).await, "strlen").call;
        scenario.step_into_to_stop(strlen).await;

        assert_eq!(place(&scenario).await, next, "{fixture}");
        scenario.shutdown().await;
    }
}

/// A call the step passes over returns to its own activation's return
/// address only after deeper activations of the same function return
/// there.
#[tokio::test]
async fn recursion_through_a_passed_call_does_not_end_it_early() {
    for fixture in ["step-targets-gcc-o0", "step-targets-clang-o0"] {
        let mut scenario = stopped_at(fixture, "targets: fact").await;

        // The outermost activation, of fact(3).
        assert_eq!(signed_variable(&scenario, "n").await, 3, "{fixture}");
        let listed = targets(&scenario).await;
        assert_eq!(callees(&listed), [Some("fact"), Some("twice")], "{fixture}");
        assert_eq!(
            scenario
                .step_into_to_stop(calling(&listed, "twice").call)
                .await,
            StopReason::Step {
                kind: StepKind::IntoSource
            },
            "{fixture}"
        );

        assert_eq!(
            place(&scenario).await,
            ("twice".to_owned(), source_line(SOURCE, "targets: twice")),
            "{fixture}"
        );

        // Not a deeper activation's call, of twice(2).
        assert_eq!(signed_variable(&scenario, "x").await, 3, "{fixture}");
        scenario.shutdown().await;
    }
}

/// Only a call of the stopped line can be stepped into, from its
/// innermost frame.
#[tokio::test]
async fn a_step_into_another_address_is_refused() {
    let mut scenario = stopped_at("step-targets-gcc-o0", "targets: calls").await;
    let listed = targets(&scenario).await;
    let call = listed[0].call;
    let elsewhere = microscope::VirtualAddress::new(call.get() + 1);
    let refused = scenario.handle().step_into(elsewhere).await;
    assert!(
        matches!(refused, Err(Error::NotAStepTarget(address)) if address == elsewhere),
        "{refused:?}"
    );

    // The program did not move.
    assert_eq!(targets(&scenario).await, listed);
    scenario.step_into_to_stop(call).await;
    scenario.shutdown().await;
}

/// A breakpoint that declines a hit in a call the step passes, by its hit
/// condition, neither stops the program nor ends the step there: the
/// passed call runs freely, as a step over runs a call.
#[tokio::test]
async fn a_declined_breakpoint_in_a_passed_call_does_not_end_the_step() {
    for fixture in BUILDS {
        let mut scenario = stopped_at(fixture, "targets: calls").await;
        let condition = microscope::HitCondition::new(microscope::HitComparison::Equal, 5)
            .expect("a hit condition");

        scenario
            .operation(
                "declining breakpoint",
                scenario.handle().add_breakpoint_with_hit_condition(
                    BreakpointSpec::Function("twice".to_owned()),
                    condition,
                ),
            )
            .await;

        let call = calling(&targets(&scenario).await, "add").call;
        assert_eq!(
            scenario.step_into_to_stop(call).await,
            StopReason::Step {
                kind: StepKind::IntoSource
            },
            "{fixture}"
        );

        assert_eq!(
            place(&scenario).await,
            ("add".to_owned(), source_line(SOURCE, "targets: add")),
            "{fixture}"
        );

        scenario.shutdown().await;
    }
}

/// A breakpoint on a call the step passes, whether the step starts there
/// or the breakpoint declines the hit on the way, does not take the step
/// into that call: stepping over its trap enters the call, which then runs
/// to its return.
#[tokio::test]
async fn a_breakpoint_on_a_passed_call_does_not_take_the_step_into_it() {
    for fixture in BUILDS {
        for declining in [false, true] {
            let mut scenario = stopped_at(fixture, "targets: calls").await;
            let listed = targets(&scenario).await;
            let (first, last) = (listed[0].call, listed[listed.len() - 1].clone());
            let at = scenario
                .operation("location", scenario.handle().current_location())
                .await
                .address;

            if declining {
                if at == first {
                    scenario.shutdown().await;
                    continue;
                }

                let condition = microscope::HitCondition::new(microscope::HitComparison::Equal, 5)
                    .expect("a hit condition");

                scenario
                    .operation(
                        "declining breakpoint",
                        scenario.handle().add_breakpoint_with_hit_condition(
                            BreakpointSpec::Address(first),
                            condition,
                        ),
                    )
                    .await;
            } else {
                scenario
                    .add_breakpoint_spec(BreakpointSpec::Address(first))
                    .await;

                if at != first {
                    assert!(
                        matches!(
                            scenario.resume_to_stop().await,
                            StopReason::Breakpoint { .. }
                        ),
                        "{fixture}"
                    );
                }
            }

            let callee = last.callee.as_deref().expect("a named callee");
            assert_eq!(
                scenario.step_into_to_stop(last.call).await,
                StopReason::Step {
                    kind: StepKind::IntoSource
                },
                "{fixture}: declining {declining}"
            );

            assert_eq!(
                place(&scenario).await,
                (
                    callee.to_owned(),
                    source_line(SOURCE, &format!("targets: {callee}"))
                ),
                "{fixture}: declining {declining}"
            );

            scenario.shutdown().await;
        }
    }
}

/// A step into a call the program then skips stops at the next line, even
/// when that line begins with another call whose breakpoint declines the
/// hit: the step reaches the line before it reaches the call.
#[tokio::test]
async fn a_skipped_call_ends_the_step_at_the_next_line_on_a_declining_breakpoint() {
    for fixture in ["step-targets-gcc-o0", "step-targets-clang-o0"] {
        let mut scenario = stopped_at(fixture, "targets: skipped").await;
        let next = source_line(SOURCE, "targets: after skipped");
        let condition = microscope::HitCondition::new(microscope::HitComparison::Equal, 5)
            .expect("a hit condition");

        scenario
            .operation(
                "declining breakpoint",
                scenario.handle().add_breakpoint_with_hit_condition(
                    BreakpointSpec::Source {
                        path: "step-targets.c".into(),
                        line: microscope::LineNumber::new(next).expect("a line"),
                    },
                    condition,
                ),
            )
            .await;
        let call = calling(&targets(&scenario).await, "add").call;
        assert_eq!(
            scenario.step_into_to_stop(call).await,
            StopReason::Step {
                kind: StepKind::IntoSource
            },
            "{fixture}"
        );

        assert_eq!(
            place(&scenario).await,
            ("main".to_owned(), next),
            "{fixture}"
        );

        scenario.shutdown().await;
    }
}
