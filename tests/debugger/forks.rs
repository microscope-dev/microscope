//! Fork and vfork children of a debugged program.

use super::*;

/// A vfork child runs in the parent's memory until it execs or exits, so
/// the traps must be lifted while it runs. It reaches `child_work`, which
/// the debugger should not stop for, and exits with 7. The parent then calls
/// `child_work` itself, which must still stop, with the child's status.
#[tokio::test]
async fn a_vfork_child_runs_breakpointed_code_untraced_and_the_parent_still_stops() {
    let mut scenario = Scenario::launch("vfork");
    scenario.add_breakpoint("child_work").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    assert_eq!(
        support::evaluate_value(&scenario, "code").await.signed(),
        7,
        "the parent's call sees the vfork child's exit status"
    );

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );

    scenario.shutdown().await;
}

/// `system` vforks, and its child execs `/bin/sh` under the parent's memory.
/// A trap in `execve`, which that child calls, must not kill it: the program
/// exits 0 only when the shell's status is a clean exit.
#[tokio::test]
async fn a_system_child_that_execs_under_a_breakpoint_is_not_killed() {
    let mut scenario = Scenario::launch("vfork");

    // libc's symbols resolve once the program has loaded it.
    scenario.add_breakpoint("main").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    scenario.add_breakpoint("execve").await;
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );

    scenario.shutdown().await;
}

/// The main thread vforks while a sibling blocks in `read`. The sibling is
/// stopped for the window and resumed after it, and its breakpoint must still
/// stop it, with its argument intact.
#[tokio::test]
async fn a_sibling_blocked_during_a_vfork_is_resumed_under_its_traps() {
    let mut scenario = Scenario::launch("vfork-threads");
    scenario.add_breakpoint("child_work").await;
    scenario.add_breakpoint("thread_work").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    assert_eq!(
        support::evaluate_value(&scenario, "value").await.signed(),
        40,
        "the sibling reaches thread_work after the vfork"
    );

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );

    scenario.shutdown().await;
}
