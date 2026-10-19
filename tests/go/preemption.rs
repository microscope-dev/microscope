//! Preemption signals that arrive while the debugger runs one thread alone:
//! one stepping over a breakpoint, or stepping by itself. The runtime sends
//! them to a goroutine locked to its thread, which parks until stopped
//! threads restart the world if it takes one, so each must wait for the
//! next continue. A stripped build waits too: its runtime's data is not
//! described, but its use of signals is the same.

use microscope::{
    BreakpointSpec, DebuggerEvent, LineNumber, SignalPolicy, StepKind, StopReason, VirtualAddress,
};
use tokio::sync::broadcast::{Receiver, error::TryRecvError};

use crate::support::{self, Scenario};

const BUILDS: [&str; 3] = ["spin-go-o0", "spin-go-o2", STRIPPED];

/// A build without debug information, whose goroutines no check can read.
const STRIPPED: &str = "spin-go-stripped";

/// How many times each test runs the spinning thread alone.
const ROUNDS: usize = 100;

/// A program spinning at a breakpoint in its loop, with SIGURG reported so
/// a test can tell the runtime sent it.
async fn spinning(fixture: &str) -> (Scenario, Receiver<DebuggerEvent>, VirtualAddress) {
    let mut scenario = if fixture == STRIPPED {
        Scenario::launch(fixture)
    } else {
        crate::invariants::checked(fixture)
    };

    scenario
        .operation(
            "report SIGURG",
            scenario.handle().set_signal_policy(
                urgent(),
                SignalPolicy {
                    stop: false,
                    print: true,
                    pass: true,
                },
            ),
        )
        .await;

    let line = support::source_line("tests/fixtures/go/spin/main.go", "// the loop");
    scenario
        .add_breakpoint_spec(BreakpointSpec::Source {
            path: "spin/main.go".into(),
            line: LineNumber::new(line).expect("one-based"),
        })
        .await;

    let events = scenario.handle().subscribe();
    let StopReason::Breakpoint {
        address: Some(address),
        ..
    } = scenario.run_to_stop().await
    else {
        panic!("{fixture}: no breakpoint stop");
    };

    (scenario, events, address)
}

fn urgent() -> u64 {
    microscope::signal_named("SIGURG").expect("SIGURG")
}

/// How many SIGURG signals were reported since the last count.
fn preemptions(events: &mut Receiver<DebuggerEvent>) -> u64 {
    let mut count = 0;
    loop {
        match events.try_recv() {
            Ok(DebuggerEvent::SignalReceived { exception, .. }) if exception.code == urgent() => {
                count += 1;
            }
            Ok(_) => {}
            Err(TryRecvError::Lagged(skipped)) => count += skipped,
            Err(_) => return count,
        }
    }
}

#[tokio::test]
async fn preemption_waits_while_a_thread_steps_over_its_breakpoint() {
    for fixture in BUILDS {
        let (mut scenario, mut events, address) = spinning(fixture).await;
        for _ in 0..ROUNDS {
            let reason = scenario.resume_to_stop().await;
            assert!(
                matches!(reason, StopReason::Breakpoint { address: Some(hit), .. } if hit == address),
                "{fixture}: {reason:?}"
            );
        }

        assert!(preemptions(&mut events) > 0, "{fixture}: no SIGURG arrived");
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn preemption_waits_while_a_thread_steps_alone() {
    for fixture in BUILDS {
        let (mut scenario, mut events, _) = spinning(fixture).await;
        for _ in 0..ROUNDS {
            // Every thread runs to the breakpoint, so signals are pending
            // again when the spinning thread steps alone.
            let reason = scenario.resume_to_stop().await;
            assert!(
                matches!(reason, StopReason::Breakpoint { .. }),
                "{fixture}: {reason:?}"
            );

            for kind in [StepKind::Instruction, StepKind::OverSource] {
                assert_eq!(
                    scenario.step_alone_to_stop(kind).await,
                    StopReason::Step { kind },
                    "{fixture}"
                );

                let location = scenario
                    .operation("location", scenario.handle().current_location())
                    .await;
                assert_eq!(
                    location
                        .image
                        .function
                        .map(|function| function.name.to_string()),
                    Some("main.spin".to_owned()),
                    "{fixture}"
                );
            }
        }

        assert!(preemptions(&mut events) > 0, "{fixture}: no SIGURG arrived");
        scenario.shutdown().await;
    }
}
