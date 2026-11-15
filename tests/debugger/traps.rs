use super::*;
use microscope::{BreakpointHit, ExceptionDisposition, SignalPolicy, ThreadId};

fn line(marker: &str) -> u64 {
    support::source_line(
        "tests/fixtures/c/trap-jump.c",
        &format!("trap-jump: {marker}"),
    )
}

fn source(marker: &str) -> BreakpointSpec {
    BreakpointSpec::Source {
        path: "trap-jump.c".into(),
        line: microscope::LineNumber::new(line(marker)).expect("a line"),
    }
}

fn hits(reason: &StopReason) -> Vec<BreakpointHit> {
    match reason {
        StopReason::Breakpoint { hits, .. } => hits.to_vec(),
        other => panic!("expected a breakpoint stop, got {other:?}"),
    }
}

async fn stopped_at_hit() -> (Scenario, ProcessId, ThreadId, microscope::Breakpoint) {
    let mut scenario = Scenario::launch("trap-jump");
    let breakpoint = scenario.add_breakpoint_spec(source("hit")).await;
    assert_eq!(hits(&scenario.run_to_stop().await).len(), 1);
    let InferiorState::Stopped {
        process_id,
        thread_id,
        ..
    } = scenario.snapshot().await.inferior
    else {
        panic!("not stopped");
    };

    (scenario, process_id, thread_id, breakpoint)
}

#[tokio::test]
async fn a_thread_moved_after_a_signal_stopped_its_step_over_a_trap_runs_with_its_siblings() {
    let (mut scenario, process, worker, _) = stopped_at_hit().await;
    support::signal_thread(process, worker, nix::libc::SIGUSR1);
    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Exception(_)
    ));

    assert_eq!(
        scenario.jump_to_stop(source("target")).await,
        StopReason::Jump
    );
    assert_eq!(
        scenario
            .resume_with_exception(ExceptionDisposition::Pass)
            .await,
        StopReason::Exited(ExitStatus::Code(100))
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn a_stop_in_a_handler_that_interrupted_a_step_over_a_trap_counts_the_trap_once() {
    let (mut scenario, process, worker, first) = stopped_at_hit().await;
    let handle = scenario.handle().clone();
    let signal = microscope::signal_named("SIGUSR1").expect("SIGUSR1");
    let quiet = SignalPolicy {
        stop: false,
        print: false,
        pass: true,
    };
    scenario
        .operation("quiet SIGUSR1", handle.set_signal_policy(signal, quiet))
        .await;
    let handler = scenario.add_breakpoint("in_handler").await;
    support::signal_thread(process, worker, nix::libc::SIGUSR1);

    assert_eq!(
        hits(&scenario.resume_to_stop().await),
        [BreakpointHit {
            breakpoint: handler.id,
            hit_count: 1,
        }]
    );
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(111))
    );
    let counted = scenario
        .snapshot()
        .await
        .breakpoints
        .iter()
        .find(|breakpoint| breakpoint.id == first.id)
        .map(|breakpoint| breakpoint.hit_count);
    assert_eq!(counted, Some(1), "the trap's hit counted once");
    scenario.shutdown().await;
}
