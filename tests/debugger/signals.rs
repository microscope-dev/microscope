//! Signal policy: which signals stop, are reported, and are delivered.

use super::*;
use microscope::{DebuggerEvent, SignalPolicy};

fn code(name: &str) -> u64 {
    microscope::signal_named(name).unwrap_or_else(|| panic!("unknown signal {name}"))
}

const QUIET: SignalPolicy = SignalPolicy {
    stop: false,
    print: false,
    pass: true,
};

#[tokio::test]
async fn signals_are_named_and_follow_gdbs_default_policy() {
    assert_eq!(microscope::signal_named("usr1"), Some(10));
    assert_eq!(
        microscope::signal_named("SIGPOLL"),
        microscope::signal_named("SIGIO")
    );
    assert_eq!(microscope::signal_name(35).as_deref(), Some("SIG35"));
    assert_eq!(microscope::signal_name(0), None);
    assert_eq!(microscope::signal_named("SIGNOPE"), None);
    assert_eq!(microscope::signal_codes().count(), 64);

    let mut scenario = Scenario::launch("signal-policy");
    let mut events = scenario.handle().subscribe();

    // SIGUSR1 and the real-time signal stop; SIGALRM, SIGURG, SIGCHLD, and
    // SIGWINCH are delivered without stopping or being reported.
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Exception(info) if info.code == code("SIGUSR1")
    ));
    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Exception(info) if info.code == 35 && info.description.starts_with("SIG35 ")
    ));

    // Every signal was delivered, so every handler ran.
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(63))
    );

    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(event, DebuggerEvent::SignalReceived { .. }),
            "{event:?}"
        );
    }

    for name in ["SIGALRM", "SIGURG", "SIGCHLD", "SIGWINCH"] {
        assert_eq!(
            scenario
                .operation("policy", scenario.handle().signal_policy(code(name)))
                .await,
            QUIET,
            "{name}"
        );
    }

    scenario.shutdown().await;
}

#[tokio::test]
async fn policies_decide_what_stops_is_reported_and_is_delivered() {
    let mut scenario = Scenario::launch("signal-policy");
    let handle = scenario.handle().clone();

    for signal in [0, 65] {
        assert!(matches!(
            handle.signal_policy(signal).await,
            Err(Error::UnknownSignal(code)) if code == signal
        ));
        assert!(matches!(
            handle.set_signal_policy(signal, QUIET).await,
            Err(Error::UnknownSignal(code)) if code == signal
        ));
    }

    let reported = SignalPolicy {
        stop: false,
        print: true,
        pass: true,
    };

    let discarded = SignalPolicy {
        stop: false,
        print: true,
        pass: false,
    };

    for (name, policy, previous) in [
        (
            "SIGUSR1",
            reported,
            SignalPolicy {
                stop: true,
                print: true,
                pass: true,
            },
        ),
        ("SIGALRM", discarded, QUIET),
        (
            "SIG35",
            SignalPolicy {
                stop: false,
                print: false,
                pass: false,
            },
            SignalPolicy {
                stop: true,
                print: true,
                pass: true,
            },
        ),
    ] {
        assert_eq!(
            scenario
                .operation("set policy", handle.set_signal_policy(code(name), policy))
                .await,
            previous,
            "{name}"
        );
        assert_eq!(
            scenario
                .operation("policy", handle.signal_policy(code(name)))
                .await,
            policy
        );
    }

    let mut events = handle.subscribe();

    // Nothing stops. SIGALRM, whose default action would kill the program,
    // and the real-time signal are discarded, so their handlers never run.
    assert_eq!(
        scenario.run_to_stop().await,
        StopReason::Exited(ExitStatus::Code(1 | 4 | 8 | 16))
    );

    let mut received = Vec::new();
    let mut process = None;

    while let Ok(event) = events.try_recv() {
        match event {
            DebuggerEvent::InferiorLaunched { process_id, .. } => process = Some(process_id),
            DebuggerEvent::SignalReceived {
                process_id,
                thread_id,
                exception,
                ..
            } => {
                assert_eq!(Some(process_id), process);
                assert_eq!(thread_id.get(), process_id.get(), "the main thread");
                received.push((exception.code, exception.description.to_string()));
            }
            _ => {}
        }
    }

    assert_eq!(
        received.iter().map(|(code, _)| *code).collect::<Vec<_>>(),
        [code("SIGUSR1"), code("SIGALRM")],
        "only signals whose policy prints are reported: {received:?}"
    );

    // raise() sends with tgkill, which names the sending process: the
    // program itself.
    let process = process.expect("the program launched").get();
    assert_eq!(
        received[0].1,
        format!("SIGUSR1 (SI_TKILL) sent by process {process}")
    );

    scenario.shutdown().await;
}

/// A step over the last line of a function returns into the middle of its
/// caller's line and single-steps to its end. There it reaches the start
/// of the next line, where a breakpoint declines its hit, with a signal
/// pending: the handler runs, and the step ends at the start of that line,
/// as it would have without the signal.
#[tokio::test]
async fn a_step_ends_at_a_declined_breakpoint_a_signal_reached_first() {
    let path = "tests/fixtures/c/signal-steps.c";
    let mut scenario = Scenario::launch("signal-steps-clang");

    scenario
        .operation(
            "quiet SIGUSR2",
            scenario.handle().set_signal_policy(code("SIGUSR2"), QUIET),
        )
        .await;

    scenario
        .add_source_breakpoint("signal-steps.c", source_line(path, "the returning line"))
        .await;

    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    scenario.remove_all_breakpoints().await;
    let before = handled(&scenario).await;
    let line = source_line(path, "the line after the signal");
    let declined = scenario
        .operation(
            "a declined breakpoint",
            scenario.handle().add_breakpoint_with_hit_condition(
                microscope::BreakpointSpec::Source {
                    path: "signal-steps.c".into(),
                    line: microscope::LineNumber::new(line).expect("a line"),
                },
                microscope::HitCondition::new(microscope::HitComparison::Equal, 100)
                    .expect("a hit condition"),
            ),
        )
        .await;

    assert_eq!(
        scenario.step_to_stop(StepKind::OverSource).await,
        StopReason::Step {
            kind: StepKind::OverSource
        }
    );

    assert_eq!(handled(&scenario).await, before + 1, "the handler ran");
    let location = scenario
        .operation("location", scenario.handle().current_location())
        .await;

    assert_eq!(
        location
            .image
            .source
            .as_ref()
            .map(|source| source.line.get()),
        Some(line),
        "{location:?}"
    );

    assert!(
        declined.locations.iter().any(|resolved| {
            [
                microscope::BreakpointLocation::Image(location.image.address),
                microscope::BreakpointLocation::Virtual(location.address),
            ]
            .contains(&resolved.location)
        }),
        "the step stopped at {}, not where the line begins: {:?}",
        location.address,
        declined.locations
    );

    scenario.shutdown().await;
}

async fn handled(scenario: &Scenario) -> i128 {
    let variable = scenario
        .operation("handled", scenario.handle().variable("handled"))
        .await;
    match available_value(&variable.state) {
        microscope::VariableValue::Scalar(ScalarValue::Signed(value)) => *value,
        value => panic!("handled is {value:?}"),
    }
}

async fn function(scenario: &Scenario) -> Option<String> {
    scenario
        .operation("location", scenario.handle().current_location())
        .await
        .image
        .function
        .map(|function| function.name.to_string())
}

#[tokio::test]
async fn steps_run_signal_handlers_without_stopping_in_them() {
    let mut scenario = Scenario::launch("signal-steps");

    scenario
        .operation(
            "quiet SIGUSR2",
            scenario.handle().set_signal_policy(code("SIGUSR2"), QUIET),
        )
        .await;

    scenario
        .add_source_breakpoint(
            "signal-steps.c",
            source_line("tests/fixtures/c/signal-steps.c", "the stepped call"),
        )
        .await;

    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    scenario.remove_all_breakpoints().await;
    assert_eq!(
        scenario.step_to_stop(StepKind::IntoSource).await,
        StopReason::Step {
            kind: StepKind::IntoSource
        }
    );
    assert_eq!(function(&scenario).await.as_deref(), Some("sum_to"));

    // Single steps cross the system call that signals the thread: its
    // handler runs to completion and the step ends after the instruction.
    let mut steps = 0;

    while handled(&scenario).await == 0 {
        steps += 1;
        assert!(steps < 256, "the signal never arrived");
        assert_eq!(
            scenario.step_to_stop(StepKind::Instruction).await,
            StopReason::Step {
                kind: StepKind::Instruction
            }
        );
        assert_eq!(function(&scenario).await.as_deref(), Some("sum_to"));
    }

    assert_eq!(
        scenario.step_to_stop(StepKind::Out).await,
        StopReason::Step {
            kind: StepKind::Out
        }
    );
    assert_eq!(function(&scenario).await.as_deref(), Some("main"));

    // Source steps cross it too, in the next two calls.
    for (kind, crossed) in [(StepKind::OverSource, 2), (StepKind::IntoSource, 3)] {
        let mut steps = 0;

        while function(&scenario).await.as_deref() != Some("sum_to") {
            steps += 1;
            assert!(steps < 16, "never entered sum_to");
            scenario.step_to_stop(StepKind::IntoSource).await;
        }

        while function(&scenario).await.as_deref() == Some("sum_to") {
            steps += 1;
            assert!(steps < 128, "never left sum_to");
            assert_eq!(scenario.step_to_stop(kind).await, StopReason::Step { kind });
            assert_ne!(
                function(&scenario).await.as_deref(),
                Some("handler"),
                "{kind:?} stopped in the handler"
            );
        }

        assert_eq!(handled(&scenario).await, crossed, "{kind:?}");
    }

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );

    scenario.shutdown().await;
}

#[tokio::test]
async fn go_preemption_signals_never_stop_the_program() {
    let mut scenario = Scenario::launch("preempt-go");
    let urgent = code("SIGURG");

    // Reporting SIGURG proves the runtime sent it while the program ran.
    scenario
        .operation(
            "report SIGURG",
            scenario.handle().set_signal_policy(
                urgent,
                SignalPolicy {
                    stop: false,
                    print: true,
                    pass: true,
                },
            ),
        )
        .await;

    let mut events = scenario.handle().subscribe();

    assert_eq!(
        scenario.run_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );

    let mut preemptions = 0_u64;

    loop {
        match events.try_recv() {
            Ok(DebuggerEvent::SignalReceived { exception, .. }) => {
                assert_eq!(exception.code, urgent);
                preemptions += 1;
            }
            Ok(_) => {}

            // So many signals arrived that this subscriber fell behind.
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(skipped)) => {
                preemptions += skipped;
            }
            Err(_) => break,
        }
    }

    // The program finishes only once it has received eight.
    assert!(preemptions >= 8, "only {preemptions} preemption signals");
    scenario.shutdown().await;
}

/// A breakpoint instruction the program executes itself is the program's
/// stop, not one of the debugger's, and the program resumes after it.
#[tokio::test]
async fn a_program_trap_stops_as_the_programs_and_resumes_after_it() {
    let mut scenario = Scenario::launch("program-trap");
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::ProgramBreakpoint { .. }
    ));

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );

    scenario.shutdown().await;
}
