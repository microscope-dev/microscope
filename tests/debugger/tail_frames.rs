use microscope::{FrameKind, StackFrame};

use super::*;

const SOURCE: &str = "tests/fixtures/c/tail-frames.c";

const VARIANTS: [&str; 3] = [
    "tail-frames-gcc-o2",
    "tail-frames-gcc-o2-dwarf4",
    "tail-frames-clang-o2",
];

fn shown(frame: &StackFrame) -> (FrameKind, String, u64) {
    (
        frame.kind,
        frame
            .function
            .as_ref()
            .map(|function| function.name.to_string())
            .unwrap_or_default(),
        frame.source.as_ref().map_or(0, |source| source.line.get()),
    )
}

fn at(kind: FrameKind, function: &str, marker: &str) -> (FrameKind, String, u64) {
    (kind, function.to_owned(), source_line(SOURCE, marker))
}

async fn frames(scenario: &Scenario) -> Vec<StackFrame> {
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;

    let main = trace
        .frames
        .iter()
        .position(|frame| shown(frame).1 == "main")
        .unwrap_or_else(|| panic!("no frame runs main: {trace:#?}"));

    trace.frames[..=main].to_vec()
}

#[tokio::test]
async fn a_backtrace_shows_the_one_chain_of_tail_calls() {
    for variant in VARIANTS {
        let mut scenario = Scenario::launch(variant);
        scenario
            .add_source_breakpoint("tail-frames.c", source_line(SOURCE, "frames: leaf"))
            .await;

        assert!(
            matches!(scenario.run_to_stop().await, StopReason::Breakpoint { .. }),
            "{variant}"
        );
        let trace = frames(&scenario).await;
        assert_eq!(
            trace.iter().map(shown).collect::<Vec<_>>(),
            [
                at(FrameKind::Physical, "leaf", "frames: leaf"),
                at(FrameKind::TailCall, "middle", "frames: middle"),
                at(FrameKind::TailCall, "top", "frames: top"),
                at(FrameKind::Physical, "main", "frames: call top"),
            ],
            "{variant}"
        );
        assert!(
            trace
                .iter()
                .enumerate()
                .all(|(level, frame)| frame.level as usize == level),
            "{variant}: {trace:#?}"
        );

        for frame in &trace[1..3] {
            assert_eq!(
                frame.symbol.as_ref().map(|symbol| &*symbol.name),
                frame.function.as_ref().map(|function| &*function.name),
                "{variant}: {frame:#?}"
            );
        }

        assert!(
            matches!(
                scenario.resume_to_stop().await,
                StopReason::Breakpoint { .. }
            ),
            "{variant}"
        );

        assert_eq!(
            frames(&scenario)
                .await
                .iter()
                .map(shown)
                .collect::<Vec<_>>(),
            [
                at(FrameKind::Physical, "leaf", "frames: leaf"),
                at(FrameKind::Physical, "main", "frames: call either"),
            ],
            "{variant}"
        );

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn a_tail_call_frame_knows_only_what_was_passed_to_it() {
    for variant in VARIANTS {
        let mut scenario = Scenario::launch(variant);
        scenario
            .add_source_breakpoint("tail-frames.c", source_line(SOURCE, "frames: leaf"))
            .await;

        scenario.run_to_stop().await;
        let trace = frames(&scenario).await;

        for (level, function, value) in [(1, "middle", 6), (2, "top", 5)] {
            let frame = &trace[level];
            scenario
                .operation(function, scenario.handle().select_frame(frame.id))
                .await;
            let registers = scenario
                .operation("registers", scenario.handle().registers())
                .await;

            let segments = ["cs", "ss", "ds", "es", "fs", "gs", "fs_base", "gs_base"];
            assert!(
                registers
                    .registers
                    .iter()
                    .filter(|register| !segments.contains(&&*register.register.name))
                    .all(|register| register.bytes.is_none()),
                "{variant}: {function}: {registers:#?}"
            );

            let variable = scenario
                .operation("value", scenario.handle().variable("value"))
                .await;

            if (variant, function) == ("tail-frames-clang-o2", "middle") {
                assert_eq!(
                    variable.state,
                    VariableState::Unavailable(VariableUnavailableReason::EntryValue(
                        microscope::EntryValueUnavailableReason::NoParameter
                    )),
                    "{variant}: {function}"
                );
                continue;
            }

            assert_eq!(
                available_value(&variable.state),
                &microscope::VariableValue::Scalar(ScalarValue::Signed(value)),
                "{variant}: {function}"
            );
        }

        scenario
            .operation("select middle", scenario.handle().select_frame(trace[1].id))
            .await;

        assert_eq!(
            scenario.step_to_stop(StepKind::Out).await,
            StopReason::Step {
                kind: StepKind::Out
            },
            "{variant}"
        );

        let location = scenario
            .operation("location", scenario.handle().current_location())
            .await;
        assert_eq!(
            location_function(&location),
            Some("main"),
            "{variant}: {location:?}"
        );

        scenario.shutdown().await;
    }
}
