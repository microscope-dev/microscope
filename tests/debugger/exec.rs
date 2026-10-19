//! Following a process through exec(2): into its own program again, and
//! from a launcher that executes the program.

use std::io::Write as _;
use std::path::Path;

use super::libraries::{breakpoint, pending_function};
use super::support::ExternalProcess;
use super::*;

fn function_name(trace: &microscope::Backtrace, index: usize) -> Option<&str> {
    trace.frames[index]
        .function
        .as_ref()
        .map(|function| &*function.name)
}

/// Stops at the program's `reexecuted` breakpoint and checks its argument
/// count.
async fn assert_reexecuted(scenario: &Scenario, reason: &StopReason, argc: i128) {
    let StopReason::Breakpoint { hits, .. } = reason else {
        panic!("stopped for {reason:?}");
    };

    assert_eq!(hits.len(), 1);
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    assert_eq!(function_name(&trace, 0), Some("reexecuted"));
    assert_eq!(function_name(&trace, 1), Some("main"));
    let count = scenario
        .operation("argc", scenario.handle().variable("argc"))
        .await;
    assert_variable_value(&count, ScalarValue::Signed(argc));
}

/// Checks that the one vDSO module is the one the executed image mapped,
/// since exec(2) maps a new vDSO, anywhere under randomization.
async fn assert_vdso_follows_the_image(scenario: &mut Scenario) {
    let InferiorState::Stopped { process_id, .. } = scenario.snapshot().await.inferior else {
        panic!("the program is stopped");
    };

    let modules = scenario
        .operation("modules", scenario.handle().loaded_modules())
        .await;
    assert_eq!(
        support::vdso_module(&modules).module.load_bias.get(),
        support::vdso_mapping(process_id).start
    );
}

#[tokio::test]
async fn a_program_that_executes_itself_again_is_followed_with_its_breakpoints() {
    let mut scenario = Scenario::launch("reexec");
    scenario.add_breakpoint("reexecuted").await;
    let library = pending_function(&scenario, "puts").await;

    assert_eq!(
        scenario.run_to_stop().await,
        StopReason::Exec { followed: true }
    );

    // The first image's libraries went with it, and the new image's dynamic
    // loader has loaded none yet.
    assert!(
        breakpoint(&mut scenario, library.id)
            .await
            .locations
            .is_empty()
    );
    scenario
        .operation("backtrace at exec", scenario.handle().backtrace())
        .await;

    let reason = scenario.resume_to_stop().await;
    assert_reexecuted(&scenario, &reason, 2).await;
    assert_vdso_follows_the_image(&mut scenario).await;
    let reason = scenario.resume_to_stop().await;
    let StopReason::Breakpoint { hits, .. } = &reason else {
        panic!("stopped for {reason:?}");
    };

    assert_eq!(hits[0].breakpoint, library.id);
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(2))
    );

    scenario.shutdown().await;
}

/// Breakpoints waiting on indirect functions' resolvers, which a program
/// may never call, wait again in the image exec(2) maps: the first image's
/// resolver sites went with it.
#[tokio::test]
async fn indirect_functions_wait_again_after_exec() {
    let mut scenario = Scenario::launch("reexec");
    for name in ["strlen", "strcasestr", "wcsnlen", "rawmemchr", "memrchr"] {
        pending_function(&scenario, name).await;
    }

    // The loader has functions of these names of its own.
    let mut reason = scenario.run_to_stop().await;
    while let StopReason::Breakpoint { .. } = reason {
        reason = scenario.resume_to_stop().await;
    }

    assert_eq!(reason, StopReason::Exec { followed: true });

    // Libraries load again, which places the waiting breakpoints anew.
    loop {
        match scenario.resume_to_stop().await {
            StopReason::Exited(status) => {
                assert_eq!(status, ExitStatus::Code(2));
                break;
            }
            StopReason::Breakpoint { .. } => {}
            other => panic!("stopped for {other:?}"),
        }
    }

    scenario.shutdown().await;
}

#[tokio::test]
async fn a_launcher_that_executes_the_program_launches_it() {
    for stop_at_entry in [false, true] {
        let mut launcher =
            ExternalProcess::exec_gate(&Scenario::fixture("reexec"), &["one", "two"]);
        let mut scenario = Scenario::new(
            format!("launch by exec, stop at entry {stop_at_entry}"),
            Scenario::fixture("reexec"),
        );
        scenario.add_breakpoint("reexecuted").await;
        let mut release = launcher.take_stdin();
        let mut reason = scenario
            .launch_by_exec_to_stop(launcher.process_id(), stop_at_entry, move || {
                release.write_all(b"\n").expect("release the launcher");
            })
            .await;
        if stop_at_entry {
            assert_eq!(reason, StopReason::Entry);

            // The launcher runs randomized, as does the program it executes.
            assert_vdso_follows_the_image(&mut scenario).await;
            reason = scenario.resume_to_stop().await;
        }

        assert_reexecuted(&scenario, &reason, 3).await;
        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(3))
        );
        launcher.reaped();
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn a_launcher_runs_as_it_would_untraced_until_it_executes_the_program() {
    let mut scenario = Scenario::launch("reexec");

    // Job control stopping it does not hold the launch back.
    let mut launcher = ExternalProcess::exec_gate(&Scenario::fixture("reexec"), &["one"]);
    let process = launcher.process_id();
    let mut release = launcher.take_stdin();
    let reason = scenario
        .launch_by_exec_to_stop(process, false, move || {
            kill(pid(process), Signal::SIGSTOP).expect("stop the launcher");
            release.write_all(b"\n").expect("release the launcher");
        })
        .await;
    assert_eq!(reason, StopReason::Exited(ExitStatus::Code(2)));
    launcher.reaped();

    // A signal it does not handle is delivered and ends it, and the launch
    // with it.
    let launcher = ExternalProcess::exec_gate(&Scenario::fixture("reexec"), &["one"]);
    let process = launcher.process_id();
    let failed = scenario
        .attempt(
            "launch by exec",
            scenario.handle().launch_by_exec(process, false, move || {
                kill(pid(process), Signal::SIGTERM).expect("terminate the launcher");
            }),
        )
        .await;
    let error = failed.expect_err("the launcher never executed the program");
    assert!(error.to_string().contains("SIGTERM"), "{error}");
    launcher.reaped();
    scenario.shutdown().await;
}

/// Launches through a launcher of `program` that is released at once, and
/// returns why the launch failed. The debugger reaps the launcher, its
/// child, before the next one starts: its waiter runs until this process has
/// no children.
async fn failed_launch(program: &Path) -> Error {
    let scenario = Scenario::launch("reexec");
    let mut launcher = ExternalProcess::exec_gate(program, &[]);
    let mut release = launcher.take_stdin();
    let failed = scenario
        .attempt(
            "launch by exec",
            scenario
                .handle()
                .launch_by_exec(launcher.process_id(), false, move || {
                    release.write_all(b"\n").expect("release the launcher");
                }),
        )
        .await;
    scenario.shutdown().await;
    launcher.reaped();

    failed.expect_err("the launch fails")
}

#[tokio::test]
async fn a_launch_through_a_process_that_cannot_execute_the_program_fails() {
    // It executes another program, which is killed.
    let error = failed_launch(&Scenario::fixture("basic")).await;
    assert!(
        error.to_string().contains("executed a program other than"),
        "{error}"
    );

    // Its exec fails, so it exits first.
    let error = failed_launch(Path::new("/nonexistent/program")).await;
    assert!(error.to_string().contains("Code(127)"), "{error}");

    // A thread it started could execute the program untraced.
    let scenario = Scenario::launch("reexec");
    let threaded = ExternalProcess::spawn(&Scenario::fixture("attach-threads"));
    let failed = scenario
        .attempt(
            "launch by exec of a threaded process",
            scenario
                .handle()
                .launch_by_exec(threaded.process_id(), false, || {}),
        )
        .await;
    assert!(
        matches!(failed, Err(Error::ProcessHasThreads(_))),
        "{failed:?}"
    );
    scenario.shutdown().await;
}

fn pid(process: ProcessId) -> Pid {
    Pid::from_raw(i32::try_from(process.get()).expect("process id"))
}
