use microscope::{ExitStatus, StopReason};

use crate::invariants;

#[tokio::test]
async fn a_child_started_by_the_runtime_execs_past_a_breakpoint_in_its_path() {
    let mut scenario = invariants::checked("exec-go-o0");
    scenario.add_breakpoint("syscall.forkAndExecInChild1").await;
    scenario
        .add_breakpoint("syscall.runtime_AfterForkInChild")
        .await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0)),
        "the child's exec failed or the child was killed"
    );
    scenario.shutdown().await;
}
